//! The infinite canvas over HTTP (`docs/CANVAS-SYNC.md`). Thin: the journal validates and stores
//! ink ops (`Journal::append_ink_batch` / `ink_events`); these handlers only move JSON.

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

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
    match state.journal().append_ink_batch(&month, body.ops) {
        Ok(evs) => Json(json!({
            "events": evs.iter().map(|e| json!({"event_id": e.event_id, "recorded_at": e.recorded_at})).collect::<Vec<_>>()
        }))
        .into_response(),
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
    let after = p.after.unwrap_or(0);
    if !memorious_core::ink::is_month_key(&month) {
        return err(StatusCode::BAD_REQUEST, "month must be YYYY-MM");
    }
    match state.journal().ink_events(&month, after) {
        Ok(evs) => Json(json!({
            "device_id": state.journal().device_id(),
            "cursor": evs.last().map(|(r, _)| *r).unwrap_or(after),
            "events": evs.iter().map(|(_, e)| json!({
                "event_id": e.event_id,
                "device_id": e.device_id,
                "recorded_at": e.recorded_at,
                "data": match &e.payload {
                    memorious_core::Payload::Infra { data, .. } => data.clone(),
                    _ => Value::Null,
                },
            })).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /api/ink` → `{months: ["2026-09", ...]}`.
pub(crate) async fn months(State(state): State<SharedState>) -> Response {
    match state.journal().ink_months() {
        Ok(m) => Json(json!({ "months": m })).into_response(),
        Err(e) => internal(e),
    }
}
