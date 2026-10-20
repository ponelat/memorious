//! Single-owner OAuth 2.1 authorization server for the MCP endpoint.
//!
//! No users: the consent page's only credential is the existing browser
//! passcode. Dynamic client registration (public clients) + PKCE S256 only.
//! Token bookkeeping is `memorious_core::oauth`; this file is the HTTP face.

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use data_encoding::BASE64URL_NOPAD;
use memorious_core::oauth;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{client_key, err, internal, SharedState};

const CODE_TTL: Duration = Duration::from_secs(60);

/// A consent the owner has granted, waiting for the client to redeem it.
pub struct PendingCode {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    issued: Instant,
}

#[derive(Default)]
pub struct Codes(StdMutex<HashMap<String, PendingCode>>);

pub fn pkce_s256(verifier: &str) -> String {
    BASE64URL_NOPAD.encode(&Sha256::digest(verifier.as_bytes()))
}

/// Public origin of this server. `MEMORIOUS_PUBLIC_URL` wins; otherwise the
/// proxy's forwarded headers (the listener is loopback-only behind Caddy).
pub fn base_url(headers: &HeaderMap) -> String {
    if let Ok(u) = std::env::var("MEMORIOUS_PUBLIC_URL") {
        return u.trim_end_matches('/').to_string();
    }
    let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap().trim());
    let host = get("x-forwarded-host").or_else(|| get("host")).unwrap_or("localhost");
    let proto = get("x-forwarded-proto")
        .unwrap_or(if host.starts_with("localhost") || host.starts_with("127.") { "http" } else { "https" });
    format!("{proto}://{host}")
}

pub async fn protected_resource(headers: HeaderMap) -> Response {
    let base = base_url(&headers);
    Json(json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
    }))
    .into_response()
}

pub async fn server_metadata(headers: HeaderMap) -> Response {
    let base = base_url(&headers);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct Registration {
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

pub async fn register(State(state): State<SharedState>, Json(r): Json<Registration>) -> Response {
    let name = r.client_name.unwrap_or_else(|| "MCP client".into());
    match oauth::register_client(&state.node.journal().store, &name, r.redirect_uris) {
        Ok(c) => (
            StatusCode::CREATED,
            Json(json!({
                "client_id": c.client_id,
                "client_name": c.name,
                "redirect_uris": c.redirect_uris,
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })),
        )
            .into_response(),
        Err(_) => oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", "redirect_uris must be https, loopback http, or an app scheme"),
    }
}

fn oauth_error(status: StatusCode, code: &str, desc: &str) -> Response {
    (status, Json(json!({"error": code, "error_description": desc}))).into_response()
}

#[derive(Deserialize, Clone)]
pub struct AuthorizeParams {
    response_type: Option<String>,
    client_id: String,
    redirect_uri: String,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    passcode: Option<String>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Validate everything about the request that doesn't involve the passcode.
/// Errors here are shown to the owner, never redirected (the redirect target
/// isn't trusted until it matches a registered one).
fn check_request(state: &SharedState, p: &AuthorizeParams) -> Result<oauth::Client, Response> {
    let bad = |m: &str| Err(err(StatusCode::BAD_REQUEST, m));
    let client = match oauth::get_client(&state.node.journal().store, &p.client_id) {
        Ok(Some(c)) => c,
        Ok(None) => return bad("unknown client"),
        Err(e) => return Err(internal(e)),
    };
    if !client.redirect_uris.contains(&p.redirect_uri) {
        return bad("redirect_uri is not registered for this client");
    }
    if p.response_type.as_deref() != Some("code") {
        return bad("response_type must be code");
    }
    if p.code_challenge_method.as_deref() != Some("S256") || p.code_challenge.as_deref().map_or(true, |c| c.len() < 43) {
        return bad("PKCE with S256 is required");
    }
    Ok(client)
}

/// CSP for the consent page: the global one forbids form posts anywhere but
/// self, and browsers apply `form-action` to the redirect that follows a
/// post — so the one registered redirect origin is allowed, nothing else.
fn consent_csp(redirect_uri: &str) -> HeaderValue {
    let (scheme, rest) = redirect_uri.split_once("://").unwrap_or(("https", ""));
    let target = if scheme == "http" || scheme == "https" {
        format!("{scheme}://{}", rest.split('/').next().unwrap_or(""))
    } else {
        format!("{scheme}:")
    };
    HeaderValue::from_str(&format!(
        "default-src 'none'; style-src 'unsafe-inline'; form-action 'self' {target}; frame-ancestors 'none'; base-uri 'none'"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("default-src 'none'"))
}

fn consent_page(client: &oauth::Client, p: &AuthorizeParams, error: Option<&str>, status: StatusCode) -> Response {
    let hidden = |n: &str, v: &str| format!("<input type=hidden name=\"{n}\" value=\"{}\">", esc(v));
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
<title>Connect to Memorious</title>\
<style>body{{font:17px Georgia,serif;max-width:26rem;margin:12vh auto;padding:0 1rem;color:#222}}\
input{{font:inherit;width:100%;padding:.6rem;margin:.6rem 0;box-sizing:border-box}}\
button{{font:inherit;padding:.6rem 1.2rem;background:#d9622b;color:#fff;border:0;border-radius:4px}}\
.e{{color:#b00}}</style>\
<h1>memor<span style=\"color:#d9622b\">/ious</span></h1>\
<p><b>{}</b> wants to read and add to your journal.</p>{}\
<form method=post action=\"/oauth/authorize\">{}{}{}{}{}\
<input type=password name=passcode placeholder=\"your passcode\" autofocus autocomplete=current-password>\
<button>Allow</button></form>",
        esc(&client.name),
        error.map(|e| format!("<p class=e>{}</p>", esc(e))).unwrap_or_default(),
        hidden("response_type", "code"),
        hidden("client_id", &p.client_id),
        hidden("redirect_uri", &p.redirect_uri),
        hidden("code_challenge", p.code_challenge.as_deref().unwrap_or("")),
        format!(
            "{}{}",
            hidden("code_challenge_method", "S256"),
            hidden("state", p.state.as_deref().unwrap_or(""))
        ),
    );
    let mut resp = (status, Html(html)).into_response();
    resp.headers_mut().insert(header::CONTENT_SECURITY_POLICY, consent_csp(&p.redirect_uri));
    resp
}

pub async fn authorize_page(State(state): State<SharedState>, Query(p): Query<AuthorizeParams>) -> Response {
    match check_request(&state, &p) {
        Ok(client) => consent_page(&client, &p, None, StatusCode::OK),
        Err(r) => r,
    }
}

pub async fn authorize_submit(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Form(p): Form<AuthorizeParams>,
) -> Response {
    let client = match check_request(&state, &p) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let who = client_key(&headers);
    if let Some(secs) = state.auth.locked_for(&who) {
        return crate::too_many(secs);
    }
    let ok = state.node.journal().check_passcode(p.passcode.as_deref().unwrap_or("")).unwrap_or(false);
    if !ok {
        state.auth.failed(&who).await;
        return consent_page(&client, &p, Some("Wrong passcode."), StatusCode::UNAUTHORIZED);
    }
    state.auth.succeeded(&who);

    let code = oauth::random_token();
    {
        let mut codes = state.oauth_codes.0.lock().unwrap();
        codes.retain(|_, c| c.issued.elapsed() < CODE_TTL);
        codes.insert(
            code.clone(),
            PendingCode {
                client_id: client.client_id.clone(),
                redirect_uri: p.redirect_uri.clone(),
                challenge: p.code_challenge.clone().unwrap_or_default(),
                issued: Instant::now(),
            },
        );
    }
    let sep = if p.redirect_uri.contains('?') { '&' } else { '?' };
    let mut to = format!("{}{sep}code={code}", p.redirect_uri);
    if let Some(s) = p.state.as_deref().filter(|s| !s.is_empty()) {
        to.push_str("&state=");
        to.push_str(&urlenc(s));
    }
    (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Deserialize)]
pub struct TokenRequest {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

pub async fn token(State(state): State<SharedState>, Form(r): Form<TokenRequest>) -> Response {
    let journal = state.node.journal();
    let invalid = |d: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", d);
    let Some(client_id) = r.client_id.as_deref() else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_client", "client_id required");
    };
    let passcode_hash = match journal.active_passcode_hash() {
        Ok(Some(h)) => h,
        Ok(None) => return invalid("no passcode is set"),
        Err(e) => return internal(e),
    };
    let tokens = match r.grant_type.as_str() {
        "authorization_code" => {
            let pending = r.code.as_deref().and_then(|c| state.oauth_codes.0.lock().unwrap().remove(c));
            let Some(p) = pending else { return invalid("unknown or already used code") };
            let verifier = r.code_verifier.as_deref().unwrap_or("");
            let pkce_ok = (43..=128).contains(&verifier.len()) && pkce_s256(verifier) == p.challenge;
            if p.issued.elapsed() >= CODE_TTL
                || p.client_id != client_id
                || Some(&p.redirect_uri) != r.redirect_uri.as_ref()
                || !pkce_ok
            {
                return invalid("code, client, redirect_uri or code_verifier mismatch");
            }
            oauth::issue(&journal.store, client_id, &passcode_hash)
        }
        "refresh_token" => {
            let Some(rt) = r.refresh_token.as_deref() else { return invalid("refresh_token required") };
            match oauth::refresh(&journal.store, rt, client_id, &passcode_hash) {
                Ok(Some(t)) => Ok(t),
                Ok(None) => return invalid("refresh token is invalid, spent or expired"),
                Err(e) => Err(e),
            }
        }
        _ => return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "authorization_code or refresh_token"),
    };
    match tokens {
        Ok(t) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({
                "access_token": t.access_token,
                "token_type": "Bearer",
                "expires_in": t.expires_in,
                "refresh_token": t.refresh_token,
            })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}
