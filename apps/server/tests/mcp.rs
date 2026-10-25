//! MCP endpoint + single-owner OAuth 2.1: discovery, dynamic client
//! registration, passcode-gated consent, PKCE token exchange, and the tools.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use memorious_core::{Journal, Node};
use memorious_server::{app, AppState, RouterState};
use serde_json::{json, Value};
use tower::ServiceExt;

const REDIRECT: &str = "http://localhost:7777/cb";

async fn test_router() -> (tempfile::TempDir, Router, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("j");
    let journal = Journal::init(&data_dir, "pw").unwrap();
    journal.set_passcode("sesame").unwrap();
    let node = Node::spawn(journal).await.unwrap();
    let state = Arc::new(AppState::new(node, None, data_dir));
    let router = app(RouterState(Arc::new(std::sync::RwLock::new(state.clone()))), None);
    (dir, router, state)
}

async fn send(router: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, body)
}

fn json_post(uri: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn form_post(uri: &str, pairs: &[(&str, &str)]) -> Request<Body> {
    let body = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn s256(verifier: &str) -> String {
    memorious_server::oauth::pkce_s256(verifier)
}

async fn register(router: &Router) -> String {
    let (st, _, body) = send(
        router,
        json_post(
            "/oauth/register",
            None,
            json!({"client_name": "test", "redirect_uris": [REDIRECT],
                   "grant_types": ["authorization_code", "refresh_token"],
                   "token_endpoint_auth_method": "none"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice::<Value>(&body).unwrap()["client_id"].as_str().unwrap().to_string()
}

fn authorize_pairs<'a>(client: &'a str, challenge: &'a str, passcode: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("response_type", "code"),
        ("client_id", client),
        ("redirect_uri", REDIRECT),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", "xyz"),
        ("passcode", passcode),
    ]
}

fn code_from(headers: &axum::http::HeaderMap) -> String {
    let loc = headers.get(header::LOCATION).unwrap().to_str().unwrap();
    assert!(loc.starts_with(REDIRECT), "{loc}");
    assert!(loc.contains("state=xyz"), "{loc}");
    loc.split(['?', '&']).find_map(|p| p.strip_prefix("code=")).unwrap().to_string()
}

async fn token(router: &Router, pairs: &[(&str, &str)]) -> (StatusCode, Value) {
    let (st, _, body) = send(router, form_post("/oauth/token", pairs)).await;
    (st, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Run the whole dance; returns (access_token, refresh_token, client_id).
async fn login(router: &Router) -> (String, String, String) {
    let client = register(router).await;
    let verifier = "v".repeat(60);
    let challenge = s256(&verifier);
    let (st, headers, _) =
        send(router, form_post("/oauth/authorize", &authorize_pairs(&client, &challenge, "sesame"))).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let code = code_from(&headers);
    let (st, tok) = token(
        router,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("client_id", &client),
            ("code_verifier", &verifier),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{tok}");
    assert_eq!(tok["token_type"], "Bearer");
    (
        tok["access_token"].as_str().unwrap().into(),
        tok["refresh_token"].as_str().unwrap().into(),
        client,
    )
}

async fn rpc(router: &Router, access: &str, method: &str, params: Value) -> Value {
    let (st, _, body) = send(
        router,
        json_post("/mcp", Some(access), json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn discovery_documents() {
    let (_d, router, _s) = test_router().await;
    let (st, _, body) = send(
        &router,
        Request::get("/.well-known/oauth-protected-resource").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(v["resource"].as_str().unwrap().ends_with("/mcp"));
    assert_eq!(v["authorization_servers"].as_array().unwrap().len(), 1);

    let (st, _, body) = send(
        &router,
        Request::get("/.well-known/oauth-authorization-server").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).unwrap();
    for k in ["issuer", "authorization_endpoint", "token_endpoint", "registration_endpoint"] {
        assert!(v[k].is_string(), "missing {k}");
    }
    assert_eq!(v["code_challenge_methods_supported"], json!(["S256"]));
}

#[tokio::test]
async fn mcp_without_token_points_at_discovery() {
    let (_d, router, _s) = test_router().await;
    let (st, headers, _) = send(&router, json_post("/mcp", None, json!({"jsonrpc":"2.0","id":1,"method":"ping"}))).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let www = headers.get(header::WWW_AUTHENTICATE).unwrap().to_str().unwrap();
    assert!(www.contains("resource_metadata="), "{www}");
}

#[tokio::test]
async fn consent_needs_the_passcode_and_a_registered_redirect() {
    let (_d, router, _s) = test_router().await;
    let client = register(&router).await;
    let challenge = s256(&"v".repeat(60));

    // Wrong passcode: no redirect, no code.
    let (st, headers, _) =
        send(&router, form_post("/oauth/authorize", &authorize_pairs(&client, &challenge, "nope"))).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(headers.get(header::LOCATION).is_none());

    // Unregistered redirect: refused outright, never bounced.
    let mut pairs = authorize_pairs(&client, &challenge, "sesame");
    pairs[2] = ("redirect_uri", "https://evil.example/cb");
    let (st, headers, _) = send(&router, form_post("/oauth/authorize", &pairs)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(headers.get(header::LOCATION).is_none());

    // GET renders the consent form with its own CSP allowing the redirect.
    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&state=xyz",
        enc(REDIRECT)
    );
    let (st, headers, body) = send(&router, Request::get(&uri).body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("passcode"));
    let csp = headers.get(header::CONTENT_SECURITY_POLICY).unwrap().to_str().unwrap();
    assert!(csp.contains("form-action") && csp.contains("localhost:7777"), "{csp}");
}

#[tokio::test]
async fn pkce_is_enforced_and_codes_are_single_use() {
    let (_d, router, _s) = test_router().await;
    let client = register(&router).await;
    let verifier = "v".repeat(60);
    let challenge = s256(&verifier);
    let (_, headers, _) =
        send(&router, form_post("/oauth/authorize", &authorize_pairs(&client, &challenge, "sesame"))).await;
    let code = code_from(&headers);
    let exchange = |v: String, code: String| {
        let (router, client) = (router.clone(), client.clone());
        async move {
            token(
                &router,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("redirect_uri", REDIRECT),
                    ("client_id", &client),
                    ("code_verifier", &v),
                ],
            )
            .await
        }
    };
    let (st, body) = exchange("w".repeat(50), code.clone()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    // A failed exchange burns the code, even for the right verifier.
    let (st, _) = exchange(verifier, code).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tools_add_search_recent_round_trip() {
    let (_d, router, state) = test_router().await;
    let (access, _, _) = login(&router).await;

    let init = rpc(&router, &access, "initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}})).await;
    assert!(init["result"]["capabilities"]["tools"].is_object());

    let list = rpc(&router, &access, "tools/list", json!({})).await;
    let names: Vec<_> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["add_entry", "search", "recent"]);

    let added = rpc(&router, &access, "tools/call", json!({"name":"add_entry","arguments":{"text":"remember the zebra crossing"}})).await;
    assert_eq!(added["result"]["isError"], false, "{added}");
    // It is a real capture in the journal.
    assert!(state.node.journal().list().unwrap().iter().any(|e| format!("{:?}", e.payload).contains("zebra crossing")));

    let found = rpc(&router, &access, "tools/call", json!({"name":"search","arguments":{"query":"zebra"}})).await;
    let text = found["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("zebra crossing"), "{text}");

    let recent = rpc(&router, &access, "tools/call", json!({"name":"recent","arguments":{"limit":5}})).await;
    assert!(recent["result"]["content"][0]["text"].as_str().unwrap().contains("zebra crossing"));

    // Empty text is a tool error, not a transport error.
    let bad = rpc(&router, &access, "tools/call", json!({"name":"add_entry","arguments":{"text":"  "}})).await;
    assert_eq!(bad["result"]["isError"], true);
}

#[tokio::test]
async fn oauth_tokens_and_passcode_do_not_cross_over() {
    let (_d, router, _s) = test_router().await;
    let (access, _, _) = login(&router).await;
    // An MCP token does not open the browser API...
    let (st, _, _) = send(
        &router,
        Request::get("/api/feed").header(header::AUTHORIZATION, format!("Bearer {access}")).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // ...and the raw passcode is not an MCP credential.
    let (st, _, _) = send(&router, json_post("/mcp", Some("sesame"), json!({"jsonrpc":"2.0","id":1,"method":"ping"}))).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn refresh_rotates_and_changing_the_passcode_revokes() {
    let (_d, router, _s) = test_router().await;
    let (access, refresh, client) = login(&router).await;

    let (st, tok) = token(&router, &[("grant_type","refresh_token"),("refresh_token",&refresh),("client_id",&client)]).await;
    assert_eq!(st, StatusCode::OK, "{tok}");
    let new_access = tok["access_token"].as_str().unwrap().to_string();
    let new_refresh = tok["refresh_token"].as_str().unwrap().to_string();
    // Old refresh token is spent.
    let (st, _) = token(&router, &[("grant_type","refresh_token"),("refresh_token",&refresh),("client_id",&client)]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let _ = access;

    // Changing the browser passcode signs every MCP client out.
    let (st, _, _) = send(
        &router,
        Request::post("/api/passcode")
            .header(header::AUTHORIZATION, "Bearer sesame")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"passcode":"new-pass-9"}).to_string()))
            .unwrap(),
    )
    .await;
    assert!(st.is_success(), "{st}");
    let (st, _, _) = send(&router, json_post("/mcp", Some(&new_access), json!({"jsonrpc":"2.0","id":1,"method":"ping"}))).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = token(&router, &[("grant_type","refresh_token"),("refresh_token",&new_refresh),("client_id",&client)]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// Same fake as core's tests: "car"/"automobile" share a vector axis.
struct Fake;
impl memorious_core::search::Embedder for Fake {
    fn model(&self) -> &str {
        "fake"
    }
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let t = t.to_lowercase();
                let hit = t.contains("car") || t.contains("automobile");
                if hit { vec![1.0, 0.0] } else { vec![0.0, 1.0] }
            })
            .collect())
    }
}

#[tokio::test]
async fn search_is_semantic_only_on_peers_with_an_embedder() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("j");
    let journal = Journal::init(&data_dir, "pw").unwrap();
    journal.set_passcode("sesame").unwrap();
    journal.capture_text("my automobile broke down").unwrap();
    journal.capture_text("bought an apple").unwrap();
    let node = Node::spawn(journal).await.unwrap();
    let state = Arc::new(AppState::new(node, None, data_dir).with_embedder(Arc::new(Fake)));
    let router = app(RouterState(Arc::new(std::sync::RwLock::new(state.clone()))), None);
    let (access, _, _) = login(&router).await;

    let ask = |q: &'static str| {
        let (router, access) = (router.clone(), access.clone());
        async move {
            let r = rpc(&router, &access, "tools/call", json!({"name":"search","arguments":{"query":q}})).await;
            r["result"]["content"][0]["text"].as_str().unwrap().to_string()
        }
    };
    // Nothing embedded yet: keyword only, "car" matches nothing.
    assert_eq!(ask("car").await, "No entries.");
    let n = memorious_server::embed_sweeper::sweep_once(&state.node.journal().store, &Fake).unwrap();
    assert_eq!(n, 2);
    let text = ask("car").await;
    assert!(text.contains("automobile") && !text.contains("apple"), "{text}");
}

#[tokio::test]
async fn api_search_reports_how_each_entry_matched() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("j");
    let journal = Journal::init(&data_dir, "pw").unwrap();
    journal.set_passcode("sesame").unwrap();
    journal.capture_text("the car is red").unwrap(); // keyword + meaning
    journal.capture_text("my automobile broke down").unwrap(); // meaning only
    let node = Node::spawn(journal).await.unwrap();
    let state = Arc::new(AppState::new(node, None, data_dir).with_embedder(Arc::new(Fake)));
    let router = app(RouterState(Arc::new(std::sync::RwLock::new(state.clone()))), None);
    memorious_server::embed_sweeper::sweep_once(&state.node.journal().store, &Fake).unwrap();

    let req = Request::get("/api/search?q=car").header(header::AUTHORIZATION, "Bearer sesame").body(Body::empty()).unwrap();
    let (st, _, body) = send(&router, req).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).unwrap();
    let m = |text: &str| {
        v["entries"].as_array().unwrap().iter().find(|e| e["text"] == text).unwrap()["match"].clone()
    };
    assert_eq!(m("the car is red")["keyword"], true);
    assert!(m("the car is red")["similarity"].is_number());
    assert_eq!(m("my automobile broke down")["keyword"], false);
    assert!(m("my automobile broke down")["similarity"].is_number());
}
