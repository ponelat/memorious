//! The infinite canvas over HTTP (`docs/CANVAS-SYNC.md`). Thin: the journal validates and stores
//! ink ops (`Journal::append_ink_batch` / `ink_events`); these handlers only move JSON.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::{err, internal, SharedState};

#[derive(Deserialize)]
pub(crate) struct AppendBody {
    ops: Vec<Value>,
}

#[derive(Deserialize)]
pub(crate) struct ListParams {
    /// The `cursor` of an earlier response (arrival order on this peer); omitted = everything.
    after: Option<i64>,
}

/// `POST /api/ink/{month}` `{ops: [...]}` → `{events: [{event_id, recorded_at}]}`.
pub(crate) async fn append(
    State(state): State<SharedState>,
    UrlPath(month): UrlPath<String>,
    Json(body): Json<AppendBody>,
) -> Response {
    match state.journal().ink_append_json(&month, body.ops) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

/// `GET /api/ink/{month}?after=<cursor>` → `{device_id, cursor, events: [{event_id, device_id, recorded_at, data}]}`,
/// events in arrival order (the client sorts by `recorded_at`, `event_id` to fold). `device_id` is this peer's own, so a
/// client can tell which ops it authored (undo/redo are per author); `cursor` is what to pass as `after` next time.
pub(crate) async fn list(
    State(state): State<SharedState>,
    UrlPath(month): UrlPath<String>,
    Query(p): Query<ListParams>,
) -> Response {
    if !memorious_core::ink::is_month_key(&month) {
        return err(StatusCode::BAD_REQUEST, "month must be YYYY-MM");
    }
    match state.journal().ink_list_json(&month, p.after.unwrap_or(0)) {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /api/ink` → `{months: ["2026-09", ...]}`.
pub(crate) async fn months(State(state): State<SharedState>) -> Response {
    match state.journal().ink_months_json() {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /api/ink/stream` — server-sent events: `ready` once connected, then an `ink` event `{"month": "YYYY-MM"}` each time
/// ink lands for a month (drawn here or synced in from a peer). A client then pulls that month with its cursor. If the
/// listener falls behind it gets `resync` (pull every month you show). Replaces polling; the client keeps a slow poll as a net.
pub(crate) async fn stream(State(state): State<SharedState>) -> Response {
    let rx = state.journal().subscribe_ink();
    let ready = tokio_stream::once(Ok::<_, Infallible>(Event::default().event("ready").data("{}")));
    let updates = BroadcastStream::new(rx).map(|m| {
        Ok::<_, Infallible>(match m {
            Ok(month) => Event::default().event("ink").data(json!({ "month": month }).to_string()),
            Err(_lagged) => Event::default().event("resync").data("{}"),
        })
    });
    Sse::new(ready.chain(updates)).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
}
