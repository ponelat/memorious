//! MCP over QUIC: any node answers the agent tools on ALPN `memorious/mcp/1`
//! to a holder of a revocable, MCP-only agent key. The key is not the journal
//! secret, so a leaked bridge can add and read text but cannot sync the log.

use memorious_core::mcp_quic::McpClient;
use memorious_core::{Journal, Node};
use serde_json::{json, Value};
use tempfile::tempdir;

async fn node() -> (tempfile::TempDir, Node) {
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    (dir, Node::spawn(j).await.unwrap())
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn text_of(reply: &Value) -> String {
    reply["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()
}

#[tokio::test]
async fn agent_ticket_gives_tools_over_quic() {
    let (_d, n) = node().await;
    let ticket = n.mint_agent_ticket("laptop bridge").await.unwrap();
    let mut c = McpClient::connect(&ticket).await.unwrap();

    let init = c.request(&rpc(1, "initialize", json!({"protocolVersion": "2025-06-18"}))).await.unwrap().unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "memorious");

    let list = c.request(&rpc(2, "tools/list", json!({}))).await.unwrap().unwrap();
    let names: Vec<_> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["add_entry", "search", "recent"]);

    let added = c
        .request(&rpc(3, "tools/call", json!({"name": "add_entry", "arguments": {"text": "quic says hello"}})))
        .await
        .unwrap()
        .unwrap();
    assert!(text_of(&added).starts_with("Recorded entry"), "{added}");
    assert_eq!(n.journal().list().unwrap().len(), 1);

    let found = c
        .request(&rpc(4, "tools/call", json!({"name": "search", "arguments": {"query": "hello"}})))
        .await
        .unwrap()
        .unwrap();
    assert!(text_of(&found).contains("quic says hello"), "{found}");

    // A notification owes no reply.
    let none = c.request(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.unwrap();
    assert!(none.is_none());
}

#[tokio::test]
async fn wrong_key_is_refused() {
    let (_d, n) = node().await;
    let ticket = n.mint_agent_ticket("x").await.unwrap();
    let forged = memorious_core::mcp_quic::AgentTicket::decode(&ticket).unwrap().with_key("not-the-key").encode();
    let refused = match McpClient::connect(&forged).await {
        Err(_) => true,
        Ok(mut c) => c.request(&rpc(1, "ping", json!({}))).await.is_err(),
    };
    assert!(refused);
    assert!(n.journal().list().unwrap().is_empty());
}

#[tokio::test]
async fn revoking_a_key_cuts_off_live_and_future_sessions() {
    let (_d, n) = node().await;
    let ticket = n.mint_agent_ticket("old laptop").await.unwrap();
    let mut live = McpClient::connect(&ticket).await.unwrap();
    assert!(live.request(&rpc(1, "ping", json!({}))).await.unwrap().is_some());

    let keys = n.agent_keys().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].name, "old laptop");
    n.revoke_agent_key(&keys[0].id).unwrap();

    assert!(live.request(&rpc(2, "ping", json!({}))).await.is_err(), "live session survived revoke");
    let fresh = match McpClient::connect(&ticket).await {
        Err(_) => true,
        Ok(mut c) => c.request(&rpc(1, "ping", json!({}))).await.is_err(),
    };
    assert!(fresh, "revoked key reconnected");
}

#[tokio::test]
async fn agent_key_does_not_open_sync() {
    let (_d, n) = node().await;
    let ticket = n.mint_agent_ticket("x").await.unwrap();
    // An agent ticket is not a journal ticket.
    assert!(memorious_core::JournalTicket::decode(&ticket).is_err());
}

/// The stdio bridge: newline-delimited JSON-RPC in, same out, and a peer that
/// is off yields a JSON-RPC error the chat client can show, not a hang.
#[tokio::test]
async fn stdio_bridge_relays_and_reports_unreachable_peers() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_d, n) = node().await;
    let ticket = n.mint_agent_ticket("bridge").await.unwrap();

    let (mut client_in, bridge_in) = tokio::io::duplex(64 * 1024);
    let (bridge_out, client_out) = tokio::io::duplex(64 * 1024);
    let t = ticket.clone();
    let task = tokio::spawn(async move { memorious_core::mcp_quic::run_stdio_bridge(&t, BufReader::new(bridge_in), bridge_out).await });
    let mut lines = BufReader::new(client_out).lines();

    client_in.write_all(format!("{}\n", rpc(1, "tools/list", json!({}))).as_bytes()).await.unwrap();
    let r: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(r["id"], 1);
    assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 3);

    // Peer goes away.
    n.shutdown().await;
    client_in.write_all(format!("{}\n", rpc(2, "tools/list", json!({}))).as_bytes()).await.unwrap();
    let r: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(r["id"], 2);
    assert!(r["error"]["message"].as_str().unwrap().contains("memorious"), "{r}");

    drop(client_in);
    task.await.unwrap().unwrap();
}

/// Needs the internet and a relay you run: a peer on a custom relay is
/// reachable by a bridge that knows only its relay route (no direct address).
/// Run: MEMORIOUS_TEST_RELAY=https://relay.example.com \
///      cargo test -p memorious-core --test mcp_quic -- --ignored
#[tokio::test]
#[ignore]
async fn reachable_through_the_custom_relay_alone() {
    use memorious_core::node::NetConfig;
    let Ok(relay) = std::env::var("MEMORIOUS_TEST_RELAY") else {
        eprintln!("MEMORIOUS_TEST_RELAY not set; skipping");
        return;
    };
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    j.set_net_config(&NetConfig {
        relay_mode: "custom".into(),
        relay_urls: vec![relay],
        public_lookup: false,
    })
    .unwrap();
    let n = Node::spawn(j).await.unwrap();
    let ticket = n.mint_agent_ticket("relay-only").await.unwrap();
    let t = memorious_core::mcp_quic::AgentTicket::decode(&ticket).unwrap();
    assert!(t.has_relay(), "ticket minted without the home relay");
    let relay_only = t.relay_only().encode();
    let mut c = McpClient::connect(&relay_only).await.unwrap();
    assert!(c.request(&rpc(1, "ping", json!({}))).await.unwrap().is_some());
}
