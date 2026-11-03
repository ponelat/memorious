//! MCP over QUIC: ALPN `memorious/mcp/1` on the same iroh endpoint as sync.
//!
//! No chat client speaks this natively; `memorious mcp-bridge` (a stdio MCP
//! server) dials it. Auth is a revocable *agent key*, deliberately not the
//! journal secret: it opens the three MCP tools and nothing else (no sync, no
//! blobs). Keys live in local `meta` as blake3 hashes — never synced.
//!
//! Wire: one bi stream per session, u32-LE length-prefixed JSON frames.
//! Client sends `{"key": ..}` first; then each request frame is answered by
//! one reply frame (notifications by an empty frame). The key is re-checked
//! on every request, so revoking cuts off live sessions.

use std::sync::{Arc, RwLock};

use anyhow::{bail, Context, Result};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::journal::Journal;
use crate::node::AddrWire;
use crate::oauth::random_token;
use crate::search::Embedder;
use crate::store::now_ms;

pub const MCP_ALPN: &[u8] = b"memorious/mcp/1";
const KEY_PREFIX: &str = "mcpkey/";
const TICKET_PREFIX: &str = "memorious-mcp-";
const CLOSE_BAD_KEY: u32 = 1;
const MAX_FRAME: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentKey {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

fn hash(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex().to_string()
}

/// Mint a key; returns the plaintext (shown once, only its hash is kept).
pub fn mint_key(journal: &Journal, name: &str) -> Result<String> {
    let key = random_token();
    let h = hash(&key);
    let rec = AgentKey { id: h[..12].to_string(), name: name.chars().take(80).collect(), created_at: now_ms() };
    journal.store.meta_set(&format!("{KEY_PREFIX}{h}"), &serde_json::to_vec(&rec)?)?;
    Ok(key)
}

pub fn key_valid(journal: &Journal, key: &str) -> bool {
    matches!(journal.store.meta_get(&format!("{KEY_PREFIX}{}", hash(key))), Ok(Some(_)))
}

pub fn list_keys(journal: &Journal) -> Result<Vec<AgentKey>> {
    let mut keys: Vec<AgentKey> =
        journal.store.meta_scan(KEY_PREFIX)?.into_iter().filter_map(|(_, v)| serde_json::from_slice(&v).ok()).collect();
    keys.sort_by_key(|k| k.created_at);
    Ok(keys)
}

pub fn revoke_key(journal: &Journal, id: &str) -> Result<()> {
    for (k, v) in journal.store.meta_scan(KEY_PREFIX)? {
        if serde_json::from_slice::<AgentKey>(&v).map(|r| r.id == id).unwrap_or(false) {
            journal.store.meta_delete(&k)?;
        }
    }
    Ok(())
}

/// What the bridge is configured with: the key plus where to dial.
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentTicket {
    key: String,
    addr: AddrWire,
}

impl AgentTicket {
    pub fn new(key: String, addr: &EndpointAddr) -> Self {
        Self { key, addr: AddrWire::from_addr(addr) }
    }
    /// The ticket as seen from another network: relay route only.
    pub fn relay_only(mut self) -> Self {
        self.addr = self.addr.relay_only();
        self
    }
    pub fn has_relay(&self) -> bool {
        self.addr.to_addr().map(|a| a.relay_urls().next().is_some()).unwrap_or(false)
    }
    pub fn with_key(mut self, key: &str) -> Self {
        self.key = key.into();
        self
    }
    pub fn encode(&self) -> String {
        let bytes = postcard::to_allocvec(self).expect("ticket serializes");
        format!("{TICKET_PREFIX}{}", data_encoding::BASE32_NOPAD.encode(&bytes).to_lowercase())
    }
    pub fn decode(s: &str) -> Result<Self> {
        let rest = s.trim().strip_prefix(TICKET_PREFIX).context("not an agent ticket (missing prefix)")?;
        let bytes = data_encoding::BASE32_NOPAD.decode(rest.to_uppercase().as_bytes()).context("ticket base32")?;
        postcard::from_bytes(&bytes).context("ticket payload")
    }
}

/// The embedder is per process (it is a local, optional model); the node sets
/// it after spawn, the handler reads it per request.
pub type SharedEmbedder = Arc<RwLock<Option<Arc<dyn Embedder>>>>;

#[derive(Clone)]
pub struct McpProto {
    pub journal: Arc<Journal>,
    pub embedder: SharedEmbedder,
}

impl std::fmt::Debug for McpProto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpProto").finish_non_exhaustive()
    }
}

fn acc(e: anyhow::Error) -> AcceptError {
    AcceptError::from_boxed(e.into())
}

impl ProtocolHandler for McpProto {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let bad = |c: &Connection| c.close(VarInt::from(CLOSE_BAD_KEY), b"bad key");
        let hello = read_frame(&mut recv).await.map_err(acc)?;
        let key = hello.as_ref().and_then(|h| h["key"].as_str()).unwrap_or("").to_string();
        if !key_valid(&self.journal, &key) {
            bad(&connection);
            return Err(acc(anyhow::anyhow!("bad agent key")));
        }
        write_frame(&mut send, &json!({"ok": true})).await.map_err(acc)?;
        while let Some(msg) = read_frame(&mut recv).await.map_err(acc)? {
            if !key_valid(&self.journal, &key) {
                bad(&connection);
                return Ok(());
            }
            let (journal, emb) = (self.journal.clone(), self.embedder.read().unwrap().clone());
            let reply = tokio::task::spawn_blocking(move || crate::mcp::handle(&journal, emb.as_deref(), &msg))
                .await
                .map_err(|e| acc(e.into()))?;
            write_frame(&mut send, &reply.unwrap_or(Value::Null)).await.map_err(acc)?;
        }
        let _ = send.finish();
        connection.closed().await;
        Ok(())
    }
}

async fn write_frame(send: &mut SendStream, v: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(v)?;
    send.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read_frame(recv: &mut RecvStream) -> Result<Option<Value>> {
    use iroh::endpoint::ReadExactError;
    let mut len = [0u8; 4];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.context("frame body")?;
    Ok(Some(serde_json::from_slice(&buf).context("frame decode")?))
}

/// Dials a peer's MCP ALPN. Needs no journal: just an endpoint and a ticket.
pub struct McpClient {
    endpoint: Endpoint,
    send: SendStream,
    recv: RecvStream,
    _conn: Connection,
}

impl McpClient {
    pub async fn connect(ticket: &str) -> Result<Self> {
        let t = AgentTicket::decode(ticket)?;
        let endpoint = Endpoint::builder(presets::N0).bind().await.context("bind iroh endpoint")?;
        let conn = tokio::time::timeout(std::time::Duration::from_secs(15), endpoint.connect(t.addr.to_addr()?, MCP_ALPN))
            .await
            .context("peer unreachable (timed out)")??;
        let (mut send, mut recv) = conn.open_bi().await?;
        write_frame(&mut send, &json!({"key": t.key})).await?;
        match read_frame(&mut recv).await {
            Ok(Some(v)) if v["ok"] == true => {}
            _ => bail!("peer refused the agent key"),
        }
        Ok(Self { endpoint, send, recv, _conn: conn })
    }

    /// One JSON-RPC message; `None` when it was a notification.
    pub async fn request(&mut self, msg: &Value) -> Result<Option<Value>> {
        write_frame(&mut self.send, msg).await?;
        match read_frame(&mut self.recv).await? {
            Some(Value::Null) => Ok(None),
            Some(v) => Ok(Some(v)),
            None => bail!("peer closed the session"),
        }
    }

    pub async fn close(self) {
        self.endpoint.close().await;
    }
}

/// MCP stdio transport ⇄ QUIC. Newline-delimited JSON-RPC on `input`/`output`
/// (what Claude Desktop/Code spawn), relayed to the peer in `ticket`. Connects
/// lazily and reconnects on the next message, so a peer that comes online later
/// just works; while it is off, requests get a JSON-RPC error, never a hang.
pub async fn run_stdio_bridge<R, W>(ticket: &str, input: R, mut output: W) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    AgentTicket::decode(ticket)?; // fail fast on a mangled ticket
    let mut lines = input.lines();
    let mut client: Option<McpClient> = None;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let outcome: Result<Option<Value>> = async {
            if client.is_none() {
                client = Some(McpClient::connect(ticket).await?);
            }
            client.as_mut().unwrap().request(&msg).await
        }
        .await;
        let reply = match outcome {
            Ok(r) => r,
            Err(e) => {
                client = None;
                msg.get("id").cloned().map(|id| crate::mcp::rpc_error(id, -32000, &format!("memorious peer unavailable: {e:#}")))
            }
        };
        if let Some(r) = reply {
            output.write_all(format!("{r}\n").as_bytes()).await?;
            output.flush().await?;
        }
    }
    if let Some(c) = client {
        c.close().await;
    }
    Ok(())
}
