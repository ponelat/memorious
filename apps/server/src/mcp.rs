//! MCP (Model Context Protocol) over streamable HTTP: one JSON-RPC POST
//! endpoint, OAuth-bearer protected. The tools themselves live in
//! `memorious_core::mcp`, shared with the QUIC transport.

use axum::body::Bytes;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use memorious_core::oauth;
use serde_json::{json, Value};

use crate::oauth::base_url;
use crate::SharedState;
use axum::extract::State;

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
    let st = state.clone();
    // Search may run the embedding model: CPU-bound and possibly queued behind
    // the backfill, so never on an async worker, and never unboundedly.
    match tokio::task::spawn_blocking(move || memorious_core::mcp::handle(st.node.journal(), st.embedder.as_deref(), &msg)).await {
        Ok(Some(reply)) => Json(reply).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(), // notification
        Err(_) => rpc_error(Value::Null, -32603, "internal error"),
    }
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response {
    Json(memorious_core::mcp::rpc_error(id, code, message)).into_response()
}
