//! Local-only OAuth bookkeeping for the MCP endpoint (single owner, no accounts).
//!
//! Registered clients and issued tokens live in the `meta` table: never
//! synced, never an event, encrypted with the rest of the database. Tokens are
//! stored only as blake3 hashes. Every token is bound to the browser-passcode
//! hash that was active when it was issued, so changing the passcode — here or
//! on any synced peer — signs every MCP client out.

use anyhow::{bail, Result};
use data_encoding::BASE64URL_NOPAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::store::{now_ms, Store};

const CLIENT_PREFIX: &str = "oauth/client/";
const ACCESS_PREFIX: &str = "oauth/at/";
const REFRESH_PREFIX: &str = "oauth/rt/";
const MAX_CLIENTS: usize = 50;
pub const ACCESS_TTL_SECS: i64 = 3600;
const REFRESH_TTL_MS: i64 = 90 * 24 * 3600 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Client {
    pub client_id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub created_at: i64,
}

#[derive(Debug)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

#[derive(Serialize, Deserialize)]
struct Grant {
    client_id: String,
    passcode_hash: String,
    expires_at: i64,
}

pub fn random_token() -> String {
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    BASE64URL_NOPAD.encode(&b)
}

fn hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// https, loopback http (native apps), or a custom app scheme. Never
/// javascript:/data:/file: and never fragments.
pub fn redirect_uri_ok(uri: &str) -> bool {
    if uri.contains('#') || uri.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once("://") else {
        return false;
    };
    match scheme {
        "https" => !rest.is_empty() && !rest.starts_with('/'),
        "http" => {
            let host = rest.split(['/', ':', '?']).next().unwrap_or("");
            matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        }
        "javascript" | "data" | "file" | "ftp" | "blob" | "vbscript" => false,
        s => s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)),
    }
}

pub fn register_client(store: &Store, name: &str, redirect_uris: Vec<String>) -> Result<Client> {
    if redirect_uris.is_empty() || !redirect_uris.iter().all(|u| redirect_uri_ok(u)) {
        bail!("invalid redirect_uris");
    }
    // Registration is open (dynamic), so cap the table: oldest clients go.
    let mut existing: Vec<Client> = store
        .meta_scan(CLIENT_PREFIX)?
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice(&v).ok())
        .collect();
    existing.sort_by_key(|c| c.created_at);
    while existing.len() >= MAX_CLIENTS {
        let old = existing.remove(0);
        store.meta_delete(&format!("{CLIENT_PREFIX}{}", old.client_id))?;
    }
    let client = Client {
        client_id: random_token()[..24].to_string(),
        name: name.chars().take(80).collect(),
        redirect_uris,
        created_at: now_ms(),
    };
    store.meta_set(&format!("{CLIENT_PREFIX}{}", client.client_id), &serde_json::to_vec(&client)?)?;
    Ok(client)
}

pub fn get_client(store: &Store, client_id: &str) -> Result<Option<Client>> {
    Ok(store
        .meta_get(&format!("{CLIENT_PREFIX}{client_id}"))?
        .and_then(|v| serde_json::from_slice(&v).ok()))
}

/// Mint a fresh access + refresh pair for `client_id`, bound to the active passcode.
pub fn issue(store: &Store, client_id: &str, passcode_hash: &str) -> Result<Tokens> {
    prune(store)?;
    let (access, refresh) = (random_token(), random_token());
    let now = now_ms();
    let put = |prefix: &str, tok: &str, ttl_ms: i64| -> Result<()> {
        let g = Grant {
            client_id: client_id.into(),
            passcode_hash: passcode_hash.into(),
            expires_at: now + ttl_ms,
        };
        store.meta_set(&format!("{prefix}{}", hash(tok)), &serde_json::to_vec(&g)?)
    };
    put(ACCESS_PREFIX, &access, ACCESS_TTL_SECS * 1000)?;
    put(REFRESH_PREFIX, &refresh, REFRESH_TTL_MS)?;
    Ok(Tokens { access_token: access, refresh_token: refresh, expires_in: ACCESS_TTL_SECS })
}

fn load(store: &Store, prefix: &str, token: &str, passcode_hash: &str) -> Result<Option<Grant>> {
    let Some(v) = store.meta_get(&format!("{prefix}{}", hash(token)))? else {
        return Ok(None);
    };
    let Ok(g) = serde_json::from_slice::<Grant>(&v) else {
        return Ok(None);
    };
    Ok((g.expires_at > now_ms() && g.passcode_hash == passcode_hash).then_some(g))
}

/// Is this a live access token issued under the *current* passcode?
pub fn check_access(store: &Store, token: &str, passcode_hash: &str) -> Result<bool> {
    Ok(load(store, ACCESS_PREFIX, token, passcode_hash)?.is_some())
}

/// Spend a refresh token (single use) for a rotated pair.
pub fn refresh(store: &Store, token: &str, client_id: &str, passcode_hash: &str) -> Result<Option<Tokens>> {
    let key = format!("{REFRESH_PREFIX}{}", hash(token));
    let grant = load(store, REFRESH_PREFIX, token, passcode_hash)?;
    store.meta_delete(&key)?; // spent whether or not it was valid for this caller
    match grant {
        Some(g) if g.client_id == client_id => Ok(Some(issue(store, client_id, passcode_hash)?)),
        _ => Ok(None),
    }
}

/// Drop expired tokens, so the table doesn't grow without bound.
fn prune(store: &Store) -> Result<()> {
    let now = now_ms();
    for prefix in [ACCESS_PREFIX, REFRESH_PREFIX] {
        for (k, v) in store.meta_scan(prefix)? {
            let dead = serde_json::from_slice::<Grant>(&v).map(|g| g.expires_at <= now).unwrap_or(true);
            if dead {
                store.meta_delete(&k)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uris() {
        for ok in ["https://claude.ai/api/mcp/auth_callback", "http://localhost:7777/cb", "http://127.0.0.1/cb", "cursor://anysphere/cb"] {
            assert!(redirect_uri_ok(ok), "{ok}");
        }
        for bad in ["http://evil.example/cb", "javascript://x", "https://a/b#frag", "/relative", "https://", "data://x"] {
            assert!(!redirect_uri_ok(bad), "{bad}");
        }
    }
}
