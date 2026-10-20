//! MCP (Model Context Protocol) over streamable HTTP: one JSON-RPC POST
//! endpoint, OAuth-bearer protected. Tools: add_entry, search, recent.
//! Deliberately no redact/delete, no media: agents may add text and read.

use axum::body::Bytes;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use memorious_core::oauth;
use serde_json::{json, Value};

use crate::oauth::base_url;
use crate::SharedState;
use axum::extract::State;

const PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

fn unauthorized(headers: &HeaderMap) -> Response {
    let www = format!(
        "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
        base_url(headers)
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, www)],
        Json(json!({"error": "invalid_token"})),
    )
        .into_response()
}

pub async fn not_allowed() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

pub async fn endpoint(State(state): State<SharedState>, headers: HeaderMap, body: Bytes) -> Response {
    let journal = state.node.journal();
    let authed = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .zip(journal.active_passcode_hash().ok().flatten())
        .map(|(tok, ph)| oauth::check_access(&journal.store, tok.trim(), &ph).unwrap_or(false))
        .unwrap_or(false);
    if !authed {
        return unauthorized(&headers);
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return rpc_error(Value::Null, -32700, "parse error");
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = id else {
        return StatusCode::ACCEPTED.into_response(); // notification
    };
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
        // Search may run the embedding model: CPU-bound and possibly queued behind
        // the backfill, so never on an async worker, and never unboundedly.
        "tools/call" => {
            let st = state.clone();
            let p = params.clone();
            match tokio::task::spawn_blocking(move || call(&st, &p)).await {
                Ok(r) => r,
                Err(_) => tool_result("internal error".into(), true),
            }
        }
        _ => return rpc_error(id, -32601, "method not found"),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})).into_response()
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

fn call(state: &SharedState, params: &Value) -> Value {
    let journal = state.node.journal();
    let args = &params["arguments"];
    let out: anyhow::Result<String> = match params["name"].as_str().unwrap_or("") {
        "add_entry" => {
            let text = args["text"].as_str().unwrap_or("");
            if text.trim().is_empty() {
                return tool_result("text must not be empty".into(), true);
            }
            journal.capture_text(text).map(|e| format!("Recorded entry {}", e.event_id))
        }
        "search" => {
            let q = args["query"].as_str().unwrap_or("");
            crate::search_entries(journal, state.embedder.as_deref(), q, &crate::fts_quote(q), limit(args)).map(|v| render(&v))
        }
        "recent" => crate::recent_entries(journal, limit(args)).map(|v| render(&v)),
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
            let when = memorious_core::export_md::format_ms_utc(ms);
            let body = e["text"].as_str().or_else(|| e["annotation"].as_str()).unwrap_or("(media without transcript)");
            format!("[{when} · {} · {}]\n{body}", e["event_id"].as_str().unwrap_or(""), e["kind"].as_str().unwrap_or(""))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
