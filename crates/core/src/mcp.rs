//! The MCP tool surface, transport-agnostic. One JSON-RPC message in, at most
//! one out. The server's HTTP `/mcp` and the QUIC `memorious/mcp/1` ALPN both
//! feed this, so every node answers identically: agents may add text and
//! read — no redact, no delete, no media.

use anyhow::Result;
use serde_json::{json, Value};

use crate::api_json::{entry_json, entry_json_annotated};
use crate::event::{EventKind, Payload};
use crate::journal::Journal;
use crate::search::Embedder;

pub const PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Handle one JSON-RPC message. `None` for a notification (no reply owed).
/// Blocking — search may run the embedding model — so async callers must go
/// through `spawn_blocking`.
pub fn handle(journal: &Journal, embedder: Option<&dyn Embedder>, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = PROTOCOLS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOLS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "memorious", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Memorious is an append-only personal journal. Use `search` to find past entries, `recent` to read the latest, and `add_entry` to record a new text note.",
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => call(journal, embedder, &params),
        _ => return Some(rpc_error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

pub fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tools() -> Value {
    json!([
        {
            "name": "add_entry",
            "description": "Append a text note to the journal. Entries are permanent and cannot be edited or deleted through this interface.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string", "description": "The note to record"}}, "required": ["text"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": false}
        },
        {
            "name": "search",
            "description": "Search journal entries (text, plus transcripts and OCR of audio and photos). Returns the best matches.",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10}}, "required": ["query"]},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "recent",
            "description": "The most recent journal entries, newest first.",
            "inputSchema": {"type": "object", "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10}}},
            "annotations": {"readOnlyHint": true}
        }
    ])
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn limit(args: &Value) -> usize {
    args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize
}

fn call(journal: &Journal, embedder: Option<&dyn Embedder>, params: &Value) -> Value {
    let args = &params["arguments"];
    let out: Result<String> = match params["name"].as_str().unwrap_or("") {
        "add_entry" => {
            let text = args["text"].as_str().unwrap_or("");
            if text.trim().is_empty() {
                return tool_result("text must not be empty".into(), true);
            }
            journal.capture_text(text).map(|e| format!("Recorded entry {}", e.event_id))
        }
        "search" => {
            let q = args["query"].as_str().unwrap_or("");
            search_entries(journal, embedder, q, &fts_quote(q), limit(args)).map(|v| render(&v))
        }
        "recent" => recent_entries(journal, limit(args)).map(|v| render(&v)),
        other => return tool_result(format!("unknown tool: {other}"), true),
    };
    match out {
        Ok(t) => tool_result(t, false),
        Err(e) => tool_result(format!("{e:#}"), true),
    }
}

/// One entry per block: `[date · id · kind]` then its text or annotation.
fn render(entries: &[Value]) -> String {
    if entries.is_empty() {
        return "No entries.".into();
    }
    entries
        .iter()
        .map(|e| {
            let ms = e["recorded_at"].as_i64().unwrap_or(0);
            let when = crate::export_md::format_ms_utc(ms);
            let body = e["text"].as_str().or_else(|| e["annotation"].as_str()).unwrap_or("(media without transcript)");
            format!("[{when} · {} · {}]\n{body}", e["event_id"].as_str().unwrap_or(""), e["kind"].as_str().unwrap_or(""))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn annotated_entry(journal: &Journal, e: &crate::Event) -> Value {
    let mut v = entry_json(e);
    if let Ok(map) = journal.annotations() {
        if let Some(text) = map.get(&e.event_id) {
            if !text.is_empty() {
                v["annotation"] = text.clone().into();
            }
        }
    }
    v
}

/// Entries matching `q`, best first. Raw FTS5 syntax, so a stray quote is an
/// error — callers that take free text from an agent should use `fts_quote`.
pub fn search_entries(
    journal: &Journal,
    embedder: Option<&dyn Embedder>,
    q: &str,
    fts_q: &str,
    limit: usize,
) -> Result<Vec<Value>> {
    let redacted = journal.store.redacted_ids()?;
    let mut out: Vec<Value> = Vec::new();
    // Over-fetch: several rows (an entry and its annotations) collapse to one.
    let fetch = limit.saturating_mul(3);
    for hit in crate::search::hybrid_search_hits(&journal.store, embedder, q, fts_q, fetch)? {
        if out.len() >= limit {
            break;
        }
        let how = json!({"keyword": hit.keyword, "similarity": hit.similarity});
        if let Some(e) = journal.store.get_event(&hit.id)? {
            // Annotations surface as their target entry.
            let display = match &e.payload {
                Payload::Annotation { target, .. } => journal.store.get_event(target)?,
                _ => Some(e),
            };
            if let Some(e) = display {
                if e.kind == EventKind::Capture && !redacted.contains(&e.event_id) {
                    // An entry and its annotations are separate rows: fold
                    // their reasons into the best-ranked one.
                    if let Some(o) = out.iter_mut().find(|o| o["event_id"] == e.event_id.as_str()) {
                        let (k, s) = (hit.keyword || o["match"]["keyword"] == true, hit.similarity.or(o["match"]["similarity"].as_f64().map(|f| f as f32)));
                        o["match"] = json!({"keyword": k, "similarity": s});
                    } else {
                        let mut entry = annotated_entry(journal, &e);
                        entry["match"] = how;
                        out.push(entry);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Free text → an FTS5 query that can't be a syntax error: every word quoted
/// (implicit AND), so punctuation and operators are just words.
pub fn fts_quote(q: &str) -> String {
    q.split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .filter(|w| w != "\"\"")
        .collect::<Vec<_>>()
        .join(" ")
}

/// Newest-first captures (the feed, minus pagination).
pub fn recent_entries(journal: &Journal, limit: usize) -> Result<Vec<Value>> {
    let annotations = journal.annotations().unwrap_or_default();
    let mut entries = journal.list()?;
    entries.reverse();
    Ok(entries.iter().take(limit).map(|e| entry_json_annotated(e, &annotations)).collect())
}
