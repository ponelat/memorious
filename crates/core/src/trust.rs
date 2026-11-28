//! Who may sync with this device: the door.
//!
//! The journal secret is what a ticket / QR code carries, so it is not proof that the person at the door is welcome: anyone
//! who photographs the code holds it. It only gets a peer to the door. The first time an endpoint id (an ed25519 key iroh
//! authenticates on every connection, so it cannot be claimed without its private key) shows up with a valid secret, it is a
//! **knock**: recorded here, answered with nothing (no heads, no events, no blobs), until the owner taps accept on *this*
//! device. Accepting binds the decision to that endpoint id, so a copy of the secret on another key is still a stranger.
//!
//! To let the owner tell a stranger's knock from their own device's, both sides show the same short **code** derived from the
//! endpoint id (`knock_code`): the joining device shows it while it waits, the approver compares it with the knock's.
//!
//! **Remove** forgets a peer completely (its allowance and every record kept about it) and, because nothing remembers it, its
//! next sync is a fresh knock. Removing does not take back what the peer already holds; rotating the whole network is the
//! stronger step and is separate.
//!
//! All of this is local meta of this device, never an event, never synced: each device decides who it talks to.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::journal::Journal;

const ALLOWED: &str = "peer_allowed:";
const KNOCK: &str = "knock:";
const REQUIRE_APPROVAL: &str = "trust_require_approval";
const GRANDFATHERED: &str = "trust_grandfathered";
/// A knock is re-sent every few seconds while the knocker waits (up to 15 minutes, `node::ASK_WAIT`); one not refreshed for 20
/// minutes has been given up on, so it is forgotten rather than offered for acceptance.
pub const KNOCK_TTL_MS: i64 = 20 * 60_000;
/// At most this many pending knocks are kept (a flood of strangers cannot grow the list without bound).
pub const MAX_KNOCKS: usize = 20;

/// Every per-peer meta key this device keeps, by prefix; `remove_peer` clears them all.
const PEER_KEYS: &[&str] = &[
    "peer_allowed:", "peer_last_ok:", "peer_device:", "peer_discovery:", "peer_heads:", "peer_media:", "peer_addr:", "peer_alive:",
    "peer_optional:", "link_ok:", "link_bad:", "knock:",
];

/// Someone at the door: an endpoint that presented the journal secret but is not (yet) allowed in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Knock {
    pub endpoint_id: String,
    /// Short code both devices show, derived from the endpoint id; compare them before accepting.
    pub code: String,
    /// The name the knocker gave itself. Anyone can claim any name: the code is what to trust.
    pub device: Option<String>,
    pub first_ms: i64,
    pub last_ms: i64,
}

/// `XXXX-XXXX` (40 bits of a hash of the endpoint id). Showing it on the joining device and on the approving one lets a person
/// tell the right knock from a stranger's; forging a matching one needs ~2^40 key generations *after* the code appears.
pub fn knock_code(endpoint_id: &str) -> String {
    let h = blake3::hash(endpoint_id.as_bytes());
    let s = data_encoding::BASE32_NOPAD.encode(&h.as_bytes()[..5]);
    format!("{}-{}", &s[..4], &s[4..8])
}

impl Journal {
    /// May this endpoint sync with us? Always true in a build with the `open-door` feature (tests) unless the journal opted in to
    /// approval with `set_require_approval`.
    pub fn is_peer_allowed(&self, endpoint_id: &str) -> Result<bool> {
        if cfg!(feature = "open-door") && self.store.meta_get(REQUIRE_APPROVAL)?.is_none() {
            return Ok(true);
        }
        Ok(self.store.meta_get(&format!("{ALLOWED}{endpoint_id}"))?.is_some())
    }

    /// Test builds only leave the door open; this makes one journal behave like a shipped build.
    pub fn set_require_approval(&self, on: bool) -> Result<()> {
        if on { self.store.meta_set(REQUIRE_APPROVAL, b"1") } else { self.store.meta_delete(REQUIRE_APPROVAL) }
    }

    /// Let `endpoint_id` in (the owner tapped accept, or this device chose to dial it). Answers its knock.
    pub fn allow_peer(&self, endpoint_id: &str) -> Result<()> {
        self.store.meta_set(&format!("{ALLOWED}{endpoint_id}"), b"1")?;
        self.store.meta_delete(&format!("{KNOCK}{endpoint_id}"))
    }

    /// Record (or refresh) a knock.
    pub fn record_knock(&self, endpoint_id: &str, device: Option<&str>, now_ms: i64) -> Result<()> {
        let key = format!("{KNOCK}{endpoint_id}");
        let prev: Option<Knock> = self.store.meta_get(&key)?.and_then(|b| serde_json::from_slice(&b).ok());
        let k = Knock {
            endpoint_id: endpoint_id.to_string(),
            code: knock_code(endpoint_id),
            device: device.map(str::to_string).or_else(|| prev.as_ref().and_then(|p| p.device.clone())),
            first_ms: prev.as_ref().map_or(now_ms, |p| p.first_ms),
            last_ms: now_ms,
        };
        self.store.meta_set(&key, &serde_json::to_vec(&k)?)?;
        // keep the newest MAX_KNOCKS
        let mut all = self.knocks(now_ms)?;
        if all.len() > MAX_KNOCKS {
            all.sort_by_key(|k| std::cmp::Reverse(k.last_ms));
            for old in &all[MAX_KNOCKS..] {
                self.store.meta_delete(&format!("{KNOCK}{}", old.endpoint_id))?;
            }
        }
        Ok(())
    }

    /// The knocks still waiting for an answer (those not refreshed within `KNOCK_TTL_MS` are dropped), newest first.
    pub fn knocks(&self, now_ms: i64) -> Result<Vec<Knock>> {
        let mut out = Vec::new();
        for (key, value) in self.store.meta_scan(KNOCK)? {
            match serde_json::from_slice::<Knock>(&value) {
                Ok(k) if now_ms - k.last_ms < KNOCK_TTL_MS => out.push(k),
                _ => self.store.meta_delete(&key)?,
            }
        }
        out.sort_by_key(|k| std::cmp::Reverse(k.last_ms));
        Ok(out)
    }

    /// The owner turned a knock away. (It may knock again.)
    pub fn reject_knock(&self, endpoint_id: &str) -> Result<()> {
        self.store.meta_delete(&format!("{KNOCK}{endpoint_id}"))
    }

    /// Forget a peer entirely: its allowance and everything this device recorded about it. Its next sync is a first knock.
    pub fn remove_peer(&self, endpoint_id: &str) -> Result<()> {
        for prefix in PEER_KEYS {
            self.store.meta_delete(&format!("{prefix}{endpoint_id}"))?;
        }
        Ok(())
    }

    /// Once, when this feature first runs on a journal: every peer already known is let in (they were trusted before there was
    /// a door). Never again, so a removed peer stays removed.
    pub fn grandfather_known_peers(&self) -> Result<()> {
        if self.store.meta_get(GRANDFATHERED)?.is_some() {
            return Ok(());
        }
        for (key, _) in self.store.meta_scan("peer_last_ok:")? {
            self.allow_peer(key.trim_start_matches("peer_last_ok:"))?;
        }
        self.store.meta_set(GRANDFATHERED, b"1")
    }
}
