//! A running peer: journal + blob store + iroh endpoint speaking the sync protocol.
//!
//! Protocol (ALPN `memorious/sync/0`), one bi stream per sync:
//!   initiator → Hello { auth, heads, addr }     auth = keyed blake3 of the journal secret
//!   responder → HelloAck { heads }              (or closes the connection on bad auth)
//!   responder → Event* EndEvents                events the initiator is missing
//!   initiator → Event* EndEvents                events the responder is missing
//!   responder → Done
//! Afterwards each side pulls any referenced-but-absent blobs over the iroh-blobs ALPN
//! (the responder dials back using the addr carried in Hello). Fetching *all* missing
//! referenced blobs each sync also heals earlier interrupted transfers.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use iroh::endpoint::{presets, Connection, ConnectionError, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_blobs::api::blobs::BlobReader;
use iroh_blobs::api::Store as BlobStore;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobsProtocol, Hash};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;
use zeroize::Zeroize;

use crate::event::{AudioKind, Event, MediaKind, Payload};
use crate::journal::{Journal, MediaHeld, SECRET_LEN};
use crate::store::Heads;

/// Blob GC cadence for real peers; see retention.rs for what GC may drop.
pub const DEFAULT_GC_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub const SYNC_ALPN: &[u8] = b"memorious/sync/1";

/// Budget for *reaching* a sync peer (dial + QUIC handshake). Iroh's own dial
/// timeout is ~30s, which every foreground sync against a dead peer would
/// otherwise pay in full. Data transfer is deliberately unbudgeted — a big
/// media sync over a slow link may take minutes.
pub const SYNC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_CONTEXT: &[u8; 32] = b"memorious auth v0 context key 32";
/// Close code used when the peer fails journal-secret auth.
const CLOSE_BAD_AUTH: u32 = 1;
const CLOSE_ISOLATED: u32 = 2;
/// The peer has the journal secret but is not (yet) allowed in: it was recorded as a knock and told nothing else (trust.rs).
const CLOSE_KNOCKING: u32 = 3;

/// Raised on the dialling side when the other device answered with a closed door: it has noted our knock and is waiting for its
/// owner to accept us. `code` is what both screens show so the owner can tell our knock from a stranger's.
#[derive(Debug)]
pub struct Knocking {
    pub code: String,
}
impl std::fmt::Display for Knocking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "waiting for approval: the other device has not accepted this one yet (code {})", self.code)
    }
}
impl std::error::Error for Knocking {}

/// A failed attempt that is just "the other device is not there (yet)": worth retrying while asking to join.
fn is_unreachable(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    m.contains("peer unreachable") || m.contains("connect to peer")
}

fn knocked_back(conn: &Connection) -> bool {
    matches!(conn.close_reason(), Some(ConnectionError::ApplicationClosed(c)) if c.error_code == VarInt::from(CLOSE_KNOCKING))
}

/// How long a device keeps asking another one to let it in (knocking, or waiting for it to come online) before it gives up.
pub const ASK_WAIT: Duration = Duration::from_secs(15 * 60);
/// Pause between two attempts while asking.
const ASK_RETRY: Duration = Duration::from_secs(3);

/// What the asking device is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AskState {
    /// The other device is not reachable (off, asleep, no network): we keep trying until it comes up.
    Unreachable,
    /// The other device answered and has our knock; its owner has to open it and accept.
    Knocking,
}

/// A request to be let in by another device, still being pinged. Process-wide: there is one at a time, and the face polls this
/// to tell the owner what is going on (and, once the other side has our knock, the code to compare there).
#[derive(Debug, Clone, Serialize)]
pub struct JoinProgress {
    pub code: String,
    pub since_ms: i64,
    /// When we stop pinging (`since_ms` + [`ASK_WAIT`]).
    pub until_ms: i64,
    pub state: AskState,
    pub attempts: u32,
}
static JOIN_PROGRESS: Mutex<Option<JoinProgress>> = Mutex::new(None);
static ASK_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `Some` while a request to join waits for the other device (to come online, or for its owner to accept).
pub fn join_progress() -> Option<JoinProgress> {
    JOIN_PROGRESS.lock().unwrap().clone()
}

/// Stop asking (the owner changed their mind). The loop ends within one retry pause.
pub fn cancel_ask() {
    ASK_CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Raised when a request to join is already being pinged: asking twice must not make a second knock.
#[derive(Debug)]
pub struct AlreadyAsking;
impl std::fmt::Display for AlreadyAsking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "already asking this device to accept this one — open it and accept")
    }
}
impl std::error::Error for AlreadyAsking {}

/// Held while a request is being pinged, so a second tap / retry / background sync cannot start another loop.
struct AskGuard;
impl AskGuard {
    fn begin(code: String) -> Result<Self> {
        let mut p = JOIN_PROGRESS.lock().unwrap();
        if p.is_some() {
            return Err(AlreadyAsking.into());
        }
        ASK_CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
        let now = unix_now_ms();
        *p = Some(JoinProgress {
            code,
            since_ms: now,
            until_ms: now + ASK_WAIT.as_millis() as i64,
            state: AskState::Unreachable,
            attempts: 0,
        });
        Ok(Self)
    }
    fn note(&self, state: AskState) {
        if let Some(p) = JOIN_PROGRESS.lock().unwrap().as_mut() {
            p.state = state;
            p.attempts += 1;
        }
    }
}
impl Drop for AskGuard {
    fn drop(&mut self) {
        *JOIN_PROGRESS.lock().unwrap() = None;
    }
}
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Postcard-friendly form of `EndpointAddr` (iroh's own serde impl needs
/// `deserialize_any`, which postcard rejects), so addresses go as strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddrWire {
    id_hex: String,
    relays: Vec<String>,
    ips: Vec<String>,
}

impl AddrWire {
    pub(crate) fn from_addr(addr: &EndpointAddr) -> Self {
        Self {
            id_hex: addr.id.to_string(),
            relays: addr.relay_urls().map(|u| u.to_string()).collect(),
            ips: addr.ip_addrs().map(|a| a.to_string()).collect(),
        }
    }

    /// Drop the direct addresses, leaving only the relay route — what a peer
    /// on another network is left with. (Tests, `#[ignore]` network ones.)
    pub fn relay_only(mut self) -> Self {
        self.ips.clear();
        self
    }

    pub(crate) fn to_addr(&self) -> Result<EndpointAddr> {
        let id: iroh::EndpointId = self.id_hex.parse().context("bad endpoint id")?;
        let mut addr = EndpointAddr::new(id);
        for r in &self.relays {
            addr = addr.with_relay_url(r.parse().context("bad relay url")?);
        }
        for ip in &self.ips {
            addr = addr.with_ip_addr(ip.parse().context("bad socket addr")?);
        }
        Ok(addr)
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum Msg {
    Hello {
        auth: [u8; 32],
        heads: Heads,
        addr: AddrWire,
        /// The sender's journal device id, so the receiver can map endpoint
        /// id → device id. Optional for wire compat with older builds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        /// What the sender holds of the journal's media (heads already say
        /// what it holds of the events). Optional for wire compat.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media: Option<MediaHeld>,
        /// `Some(false)`: the sender is a write-only peer and wants no events sent to it. Optional for wire compat (an older
        /// peer ignores it and sends anyway; a write-only peer discards what it didn't ask for).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receive: Option<bool>,
        /// The sender's alive slots (UTC 5-minute slots, last 7 days), see alive.rs. Optional for wire compat.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alive: Option<crate::alive::Slots>,
    },
    HelloAck {
        heads: Heads,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media: Option<MediaHeld>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receive: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alive: Option<crate::alive::Slots>,
    },
    Event(Event),
    EndEvents,
    Done,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct SyncReport {
    pub sent: usize,
    pub received: usize,
    pub blobs_fetched: usize,
}

/// Outcome of syncing one remembered peer. A failure never hides other outcomes.
#[derive(Debug, Serialize)]
pub struct PeerSync {
    pub endpoint_id: String,
    pub ok: bool,
    pub sent: usize,
    pub received: usize,
    pub blobs: usize,
    pub error: Option<String>,
}

/// Per-device network configuration, stored in journal meta (local, never
/// syncs — each device picks its own relays) and applied at [`Node::spawn`],
/// i.e. changes take effect on the next launch/restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetConfig {
    /// "default" = n0's public relays, "custom" = `relay_urls`, "disabled" =
    /// direct connections only (LAN / tickets with reachable addresses).
    pub relay_mode: String,
    pub relay_urls: Vec<String>,
    /// Publish/resolve peer addresses via the public iroh address lookup
    /// (DNS + pkarr records on the mainline DHT infrastructure). Off = peers
    /// are found only through tickets and last-known addresses.
    pub public_lookup: bool,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            relay_mode: "default".into(),
            relay_urls: Vec::new(),
            public_lookup: true,
        }
    }
}

const NET_CONFIG_KEY: &str = "net_config";

impl Journal {
    /// This device's network configuration (defaults when never set).
    pub fn net_config(&self) -> NetConfig {
        self.store
            .meta_get(NET_CONFIG_KEY)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Validate and store the network configuration. Applied on next spawn.
    pub fn set_net_config(&self, cfg: &NetConfig) -> Result<()> {
        match cfg.relay_mode.as_str() {
            "default" | "disabled" => {}
            "custom" => {
                if cfg.relay_urls.is_empty() {
                    bail!("custom relay mode needs at least one relay url");
                }
                for url in &cfg.relay_urls {
                    let _: iroh::RelayUrl = url
                        .parse()
                        .with_context(|| format!("bad relay url: {url}"))?;
                }
            }
            other => bail!("unknown relay mode: {other} (default/custom/disabled)"),
        }
        self.store
            .meta_set(NET_CONFIG_KEY, &serde_json::to_vec(cfg)?)
    }
}

/// The data transport to a peer, as reported on the status screen.
#[derive(Debug, Clone, Serialize)]
pub struct PeerConn {
    /// "relay" or "direct".
    pub transport: String,
    /// The relay url or the remote socket address.
    pub detail: String,
    /// Direct over a private/link-local address — the same LAN.
    pub lan: bool,
    /// Data flows through a middleman (the relay server forwards our
    /// ciphertext). False = genuine peer-to-peer. A standby relay path next
    /// to an active direct one does not count as a proxy in the chain.
    pub proxied: bool,
}

/// A known sync peer: everything the status screens show about it. Stale by
/// design — a peer's row is only as fresh as our last contact with it.
#[derive(Debug, Clone, Serialize)]
pub struct PeerInfo {
    pub endpoint_id: String,
    /// The peer's journal device id, once it has told us (Hello/HelloAck).
    pub device_id: Option<String>,
    /// Last completed event sync, unix ms.
    pub last_ok_ms: i64,
    /// How this peer entered our world: "ticket" (we redeemed a pairing
    /// ticket carrying its address) or "inbound" (it discovered us and
    /// connected in). Recorded at first contact, never rewritten.
    pub discovery: Option<String>,
    /// The transport in use right now, when the endpoint still holds a live
    /// path to the peer; `None` between contacts.
    pub conn: Option<PeerConn>,
    /// Per-device heads the peer was holding at our last completed
    /// handshake (`None` = synced only before this build).
    pub heads: Option<Heads>,
    /// Sum of `heads` — "the peer has x events"; pair with our own total.
    pub events_held: Option<u64>,
    /// Events we hold that the peer lacked as of last contact.
    pub events_missing: Option<u64>,
    /// The peer's own media tally at last contact.
    pub media: Option<MediaHeld>,
    /// Referenced media the peer lacked beyond its retention policy.
    pub media_missing: Option<u64>,
    /// Whether the health light waits on this peer (default yes; a local
    /// per-device mark, not synced).
    pub required: bool,
    /// The newest user-visible event within the heads it held at last contact — "newest it held".
    pub newest_held: Option<crate::peer_view::NewestEvent>,
    /// A sync with this peer is in flight right now.
    pub syncing: bool,
    /// When we last tried to sync with it (worked or not), unix ms.
    pub last_attempt_ms: Option<i64>,
    /// Why the last attempt failed; `None` when it worked (or nothing has been tried since launch).
    pub last_error: Option<String>,
}

/// Private, loopback, or link-local — "same network" for the status UI.
fn is_lan_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.to_ipv4_mapped().is_some_and(|v4| {
                    v4.is_private() || v4.is_loopback() || v4.is_link_local()
                })
        }
    }
}

/// Ticket: journal secret + address of one existing peer. String form `memorious<base32>`.
#[derive(Debug, Serialize, Deserialize)]
pub struct JournalTicket {
    pub secret: [u8; SECRET_LEN],
    addr: AddrWire,
}

impl JournalTicket {
    pub fn new(secret: [u8; SECRET_LEN], addr: &EndpointAddr) -> Self {
        Self {
            secret,
            addr: AddrWire::from_addr(addr),
        }
    }

    pub fn addr(&self) -> Result<EndpointAddr> {
        self.addr.to_addr()
    }
}

const TICKET_PREFIX: &str = "memorious";

impl JournalTicket {
    pub fn encode(&self) -> String {
        let bytes = postcard::to_allocvec(self).expect("ticket serializes");
        format!(
            "{TICKET_PREFIX}{}",
            data_encoding::BASE32_NOPAD.encode(&bytes).to_lowercase()
        )
    }

    pub fn decode(s: &str) -> Result<Self> {
        let rest = s
            .trim()
            .strip_prefix(TICKET_PREFIX)
            .context("not a journal ticket (missing prefix)")?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(rest.to_uppercase().as_bytes())
            .context("ticket base32")?;
        Ok(postcard::from_bytes(&bytes).context("ticket payload")?)
    }
}

fn auth_token(secret: &[u8; SECRET_LEN]) -> [u8; 32] {
    *blake3::keyed_hash(AUTH_CONTEXT, secret).as_bytes()
}

/// What each known peer is doing right now, from our side: a sync in flight, and the outcome of the last attempt. Plus
/// when the next scheduled sync (set by whoever runs the ping loop) and any debounced "push soon" nudge will fire.
#[derive(Default)]
struct SyncState {
    attempts: Mutex<HashMap<String, Attempt>>,
    schedule: Mutex<Schedule>,
}

#[derive(Default, Clone)]
struct Attempt {
    in_flight: u32,
    last_attempt_ms: Option<i64>,
    last_error: Option<String>,
}

#[derive(Default, Clone, Copy)]
struct Schedule {
    next_ms: i64,
    interval_ms: u64,
    nudge_at_ms: i64,
    nudge_gen: u64,
}

/// Marks a sync as in flight; whatever way the future ends (finished, errored, dropped by a timeout) it records the outcome.
struct AttemptGuard<'a> {
    state: &'a SyncState,
    peer: String,
    outcome: Option<Option<String>>,
}

impl<'a> AttemptGuard<'a> {
    fn begin(state: &'a SyncState, peer: String) -> Self {
        state.attempts.lock().unwrap().entry(peer.clone()).or_default().in_flight += 1;
        Self { state, peer, outcome: None }
    }
    fn finish(mut self, error: Option<String>) {
        self.outcome = Some(error);
    }
}

impl Drop for AttemptGuard<'_> {
    fn drop(&mut self) {
        let error = self.outcome.take().unwrap_or_else(|| Some("no answer in time".into()));
        let mut map = self.state.attempts.lock().unwrap();
        let a = map.entry(self.peer.clone()).or_default();
        a.in_flight = a.in_flight.saturating_sub(1);
        a.last_attempt_ms = Some(unix_now_ms());
        a.last_error = error;
    }
}

pub struct Node {
    sync_state: SyncState,
    journal: Arc<Journal>,
    blobs: BlobStore,
    endpoint: Endpoint,
    router: Router,
    _fs_store: FsStore,
    /// One blob download at a time, and a pause after a hash the peer reset.
    fetch_gate: Arc<FetchGate>,
    embedder: crate::mcp_quic::SharedEmbedder,
}

/// Why [`Node::open_blob`] refused.
#[derive(Debug)]
pub enum BlobOpenError {
    /// No capture event references this hash.
    Unreferenced,
    /// The hash is malformed, the key will not unwrap, or the blob is not here.
    Unavailable(anyhow::Error),
}

impl std::fmt::Display for BlobOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreferenced => write!(f, "no capture references this blob"),
            Self::Unavailable(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for BlobOpenError {}

/// A stored blob opened for chunked decrypt. `len` is the plaintext size.
pub struct OpenedBlob {
    pub kind: MediaKind,
    pub len: u64,
    pub plaintext: BlobPlaintext,
}

/// Plaintext chunks of one blob. Each item is at most [`crate::crypto::CHUNK_LEN`]
/// bytes; the ciphertext of that chunk is dropped before the next one is read.
pub struct BlobPlaintext {
    reader: Pin<Box<BlobReader>>,
    ck: [u8; crate::crypto::KEY_LEN],
    nonce_base: [u8; crate::crypto::NONCE_BASE_LEN],
    index: u32,
    n_chunks: u32,
    plaintext_len: u64,
    buf: Vec<u8>,
    filled: usize,
    ct_len: usize,
    done: bool,
    failed: bool,
}

fn chunk_ct_len(index: u32, n_chunks: u32, plaintext_len: u64) -> usize {
    let pt = if index + 1 == n_chunks {
        plaintext_len - index as u64 * crate::crypto::CHUNK_LEN as u64
    } else {
        crate::crypto::CHUNK_LEN as u64
    };
    pt as usize + crate::crypto::TAG_LEN
}

impl Drop for BlobPlaintext {
    fn drop(&mut self) {
        self.ck.zeroize();
        self.buf.zeroize();
    }
}

impl n0_future::Stream for BlobPlaintext {
    type Item = Result<Vec<u8>>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done || this.failed {
            return Poll::Ready(None);
        }
        loop {
            if this.filled < this.ct_len {
                let n = {
                    let mut read_buf =
                        tokio::io::ReadBuf::new(&mut this.buf[this.filled..this.ct_len]);
                    match this.reader.as_mut().poll_read(cx, &mut read_buf) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(err)) => {
                            this.failed = true;
                            return Poll::Ready(Some(Err(
                                anyhow::Error::from(err).context("read blob"),
                            )));
                        }
                        Poll::Ready(Ok(())) => read_buf.filled().len(),
                    }
                };
                if n == 0 {
                    this.failed = true;
                    return Poll::Ready(Some(Err(anyhow::anyhow!(
                        "blob ended during chunk {}",
                        this.index
                    ))));
                }
                this.filled += n;
                continue;
            }

            let is_final = this.index + 1 == this.n_chunks;
            let pt = match crate::crypto::open_chunk(
                &this.buf[..this.ct_len],
                &this.ck,
                &this.nonce_base,
                this.index,
                is_final,
            ) {
                Ok(pt) => pt,
                Err(err) => {
                    this.failed = true;
                    return Poll::Ready(Some(Err(err)));
                }
            };
            this.buf[..this.ct_len].zeroize();
            this.index += 1;
            if this.index == this.n_chunks {
                this.done = true;
                this.buf.clear();
            } else {
                this.ct_len = chunk_ct_len(this.index, this.n_chunks, this.plaintext_len);
                this.buf.resize(this.ct_len, 0);
                this.filled = 0;
            }
            return Poll::Ready(Some(Ok(pt)));
        }
    }
}

impl Node {
    /// Bind an endpoint, register sync + blobs protocols, and start accepting.
    pub async fn spawn(journal: Journal) -> Result<Self> {
        Self::spawn_with_gc_interval(journal, DEFAULT_GC_INTERVAL).await
    }

    /// `spawn` with an explicit blob-GC cadence (tests use a fast one).
    pub async fn spawn_with_gc_interval(journal: Journal, gc_interval: Duration) -> Result<Self> {
        journal.grandfather_known_peers()?;
        let journal = Arc::new(journal);
        let fs_store = {
            // Retention: the event log is the only GC root. Before each GC pass
            // iroh-blobs asks us which hashes are live; we answer "everything
            // the log references, minus what retention.rs says this device may
            // drop". Ingest tags are removed once the capture event exists
            // (see `drop_ingest_tag`), so a tag only ever protects an
            // in-flight capture.
            let protect_journal = journal.clone();
            let cb: iroh_blobs::store::ProtectCb = Arc::new(move |live: &mut HashSet<Hash>| {
                let journal = protect_journal.clone();
                Box::pin(async move {
                    match live_blob_hashes(&journal) {
                        Ok(hashes) => {
                            live.extend(hashes);
                            iroh_blobs::store::ProtectOutcome::Continue
                        }
                        Err(err) => {
                            tracing::warn!("retention: skipping GC pass: {err:#}");
                            iroh_blobs::store::ProtectOutcome::Abort
                        }
                    }
                })
            });
            let root = journal.blobs_dir();
            let mut options = iroh_blobs::store::fs::options::Options::new(&root);
            options.gc = Some(iroh_blobs::store::GcConfig {
                interval: gc_interval,
                add_protected: Some(cb),
            });
            FsStore::load_with_opts(root.join("blobs.db"), options).await?
        };
        let blobs: BlobStore = fs_store.clone().into();
        drop_ingest_tags_for_referenced(&blobs, &journal).await?;

        // Endpoint identity is per-device and persistent, so peers can re-dial us.
        let secret_key = match journal.store.meta_get("endpoint_secret")? {
            Some(bytes) => {
                let arr: [u8; 32] = bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("malformed endpoint secret"))?;
                SecretKey::from_bytes(&arr)
            }
            None => {
                let key = SecretKey::generate();
                journal.store.meta_set("endpoint_secret", &key.to_bytes())?;
                key
            }
        };

        // Network shape comes from the journal's (per-device) net config.
        // presets::N0 minus whatever the owner turned off.
        let cfg = journal.net_config();
        let relay_mode = match cfg.relay_mode.as_str() {
            "disabled" => iroh::RelayMode::Disabled,
            "custom" => iroh::RelayMode::custom(
                cfg.relay_urls
                    .iter()
                    .map(|u| u.parse().with_context(|| format!("bad relay url: {u}")))
                    .collect::<Result<Vec<_>>>()?,
            ),
            _ => iroh::endpoint::default_relay_mode(),
        };
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(secret_key)
            .relay_mode(relay_mode);
        if cfg.public_lookup {
            use iroh::address_lookup::{DnsAddressLookup, PkarrPublisher, PkarrResolver};
            builder = builder
                .address_lookup(PkarrPublisher::n0_dns())
                .address_lookup(PkarrResolver::n0_dns())
                .address_lookup(DnsAddressLookup::n0_dns());
        }
        let endpoint = builder.bind().await.context("bind iroh endpoint")?;

        let fetch_gate = Arc::new(FetchGate::new());
        let proto = SyncProto {
            journal: journal.clone(),
            blobs: blobs.clone(),
            endpoint: endpoint.clone(),
            fetch_gate: fetch_gate.clone(),
        };
        let blobs_proto = Gated { inner: BlobsProtocol::new(&blobs, None), journal: journal.clone() };
        let embedder: crate::mcp_quic::SharedEmbedder = Arc::default();
        let router = Router::builder(endpoint.clone())
            .accept(SYNC_ALPN, proto)
            .accept(iroh_blobs::ALPN, blobs_proto)
            .accept(crate::mcp_quic::MCP_ALPN, crate::mcp_quic::McpProto { journal: journal.clone(), embedder: embedder.clone() })
            .spawn();

        // "Alive" heartbeat for the Peers page: mark this device alive in the current 5-minute slot, now and every minute
        // while the node exists (the task ends when the journal is dropped).
        let _ = journal.touch_alive(unix_now_ms());
        {
            let weak = Arc::downgrade(&journal);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(60));
                loop {
                    tick.tick().await;
                    match weak.upgrade() {
                        Some(j) => {
                            let _ = j.touch_alive(unix_now_ms());
                        }
                        None => break,
                    }
                }
            });
        }

        Ok(Self {
            sync_state: SyncState::default(),
            journal,
            blobs,
            endpoint,
            router,
            _fs_store: fs_store,
            fetch_gate,
            embedder,
        })
    }

    /// Redeem a ticket: create the local journal from its secret, pull the
    /// event log, and prove the master password — but leave media blobs for a
    /// later [`Self::sync_with`], so joining a media-heavy journal is usable
    /// immediately. The ticket authorizes replication; the password (entered
    /// separately, never in the ticket) authorizes reading — proven by
    /// unwrapping a media key from the event log, so a typo can't silently
    /// produce a journal whose media never decrypts.
    pub async fn pair_from_ticket(
        root: &Path,
        ticket: &str,
        password: &str,
    ) -> Result<(Self, SyncReport)> {
        let ticket = JournalTicket::decode(ticket)?;
        let peer_addr = ticket.addr()?;
        let journal = Journal::init_with_secret(root, ticket.secret, password)?;
        // From here on a failure must not leave the half-made journal behind:
        // `init_with_secret` refuses to run over an existing db.sqlite, so a
        // peer that was unreachable (or a mistyped password) would otherwise
        // turn every retry into "journal already exists" and, on next launch,
        // into an empty journal that asks to be unlocked.
        let node = match Self::spawn(journal).await {
            Ok(node) => node,
            Err(e) => {
                Journal::discard_created(root);
                return Err(e);
            }
        };
        match Self::pull_and_prove(&node, &peer_addr).await {
            Ok(report) => Ok((node, report)),
            Err(e) => {
                node.shutdown().await;
                Journal::discard_created(root);
                Err(e)
            }
        }
    }

    /// Keep asking the device at `addr` to let this one in, for up to [`ASK_WAIT`]: it may be off (we retry until it is reachable)
    /// and its owner has to open it and tap accept (it holds our knock meanwhile). Idempotent: one loop at a time per process, a
    /// second call fails with [`AlreadyAsking`] rather than knocking again; and every attempt comes from this node's one endpoint,
    /// which the other side keys its knock by, so it only ever holds one. Returns the first sync report that goes through.
    pub async fn ask_to_join(&self, addr: &EndpointAddr) -> Result<SyncReport> {
        let guard = AskGuard::begin(crate::trust::knock_code(&self.endpoint.id().to_string()))?;
        let started = std::time::Instant::now();
        loop {
            // an attempt can take 10s to give up on an offline peer: watch for "stop asking" meanwhile
            let attempt = self.sync_events_with(addr);
            tokio::pin!(attempt);
            let result = loop {
                tokio::select! {
                    r = &mut attempt => break r,
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        if ASK_CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
                            bail!("cancelled");
                        }
                    }
                }
            };
            match result {
                Ok(r) => return Ok(r),
                Err(e) => {
                    let state = if e.downcast_ref::<Knocking>().is_some() {
                        AskState::Knocking
                    } else if is_unreachable(&e) {
                        AskState::Unreachable
                    } else {
                        return Err(e);
                    };
                    guard.note(state);
                    if ASK_CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
                        bail!("cancelled");
                    }
                    if started.elapsed() >= ASK_WAIT {
                        return Err(match state {
                            AskState::Knocking => anyhow::anyhow!("not accepted within 15 minutes: open the other device and accept this one, then try again"),
                            AskState::Unreachable => e.context("the other device never came online within 15 minutes"),
                        });
                    }
                    tokio::time::sleep(ASK_RETRY).await;
                }
            }
        }
    }

    /// The network half of pairing: pull the event log, then prove the master
    /// password by unwrapping one media key from it.
    async fn pull_and_prove(node: &Self, peer_addr: &EndpointAddr) -> Result<SyncReport> {
        let report = node.ask_to_join(peer_addr).await?;
        if !node.journal.prove_password()? {
            bail!(
                "this journal carries no password proof yet — open it once on the device \
                 that created it (with a current build), then join again"
            );
        }
        Ok(report)
    }

    /// Join a network as a **write-only** peer: a full peer in code that pulls nothing. It takes the ticket's journal secret (and
    /// the master password for its own storage and media keys), keeps its own events, and pushes them when it syncs. Bring it up,
    /// capture, `sync_events_with` the ticket's address, then throw the data directory away: the events live on in the network.
    /// Unlike a normal join it does not verify the password against the network's proofs (that would mean pulling).
    pub async fn join_write_only(root: &Path, ticket: &str, password: &str) -> Result<Self> {
        let ticket = JournalTicket::decode(ticket)?;
        let journal = Journal::init_with_secret(root, ticket.secret, password)?;
        journal.set_write_only(true)?;
        Self::spawn(journal).await
    }

    /// [`Self::pair_from_ticket`] plus the full media fetch, for callers that
    /// want everything before returning (CLI join).
    pub async fn join_from_ticket(
        root: &Path,
        ticket: &str,
        password: &str,
    ) -> Result<(Self, SyncReport)> {
        let (node, mut report) = Self::pair_from_ticket(root, ticket, password).await?;
        let peer_addr = JournalTicket::decode(ticket)?.addr()?;
        report.blobs_fetched = fetch_missing_blobs(
            &node.endpoint,
            &node.blobs,
            &node.journal,
            &peer_addr,
            &node.fetch_gate,
        )
        .await
        .context("fetch blobs")?;
        Ok((node, report))
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The ping loop tells the node when the next scheduled sync is due, so the Peers page can say "sends in 4 min".
    pub fn set_sync_schedule(&self, next_ms: i64, interval_ms: u64) {
        let mut sc = self.sync_state.schedule.lock().unwrap();
        sc.next_ms = next_ms;
        sc.interval_ms = interval_ms;
    }

    /// Announce a "push soon" after `delay` (new local data). Returns a generation; only the latest nudge of a burst should
    /// act, which it asks with [`Self::nudge_take`] once the delay has passed.
    pub fn nudge_begin(&self, delay: Duration) -> u64 {
        let mut sc = self.sync_state.schedule.lock().unwrap();
        sc.nudge_gen += 1;
        sc.nudge_at_ms = unix_now_ms() + delay.as_millis() as i64;
        sc.nudge_gen
    }

    /// True (and the nudge is cleared) when `gen` is still the latest nudge.
    pub fn nudge_take(&self, gen: u64) -> bool {
        let mut sc = self.sync_state.schedule.lock().unwrap();
        if sc.nudge_gen == gen {
            sc.nudge_at_ms = 0;
            true
        } else {
            false
        }
    }

    /// This node's endpoint id (what peers know us by).
    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.endpoint.id()
    }

    /// Our current address, waiting briefly for direct addresses to be discovered.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Wait until the address is dialable (has at least one transport addr).
    pub async fn dialable_addr(&self) -> Result<EndpointAddr> {
        for _ in 0..100 {
            let addr = self.endpoint.addr();
            if !addr.addrs.is_empty() {
                return Ok(addr);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        bail!("endpoint never became dialable");
    }

    /// Opt this node's MCP-over-QUIC search into meaning-based matching.
    pub fn set_embedder(&self, e: Arc<dyn crate::search::Embedder>) {
        *self.embedder.write().unwrap() = Some(e);
    }

    /// Mint a revocable MCP-only agent key and return the ticket a bridge
    /// dials with. The key is not the journal secret: it cannot sync.
    pub async fn mint_agent_ticket(&self, name: &str) -> Result<String> {
        let key = crate::mcp_quic::mint_key(&self.journal, name)?;
        let mut addr = self.dialable_addr().await?;
        // A ticket without the home relay only works on the LAN. The relay is
        // learned a moment after the first direct address, so wait a little
        // (best effort — a relay-less config, or an offline host, just skips).
        if self.journal.net_config().relay_mode != "disabled" {
            for _ in 0..60 {
                if addr.relay_urls().next().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                addr = self.endpoint.addr();
            }
        }
        Ok(crate::mcp_quic::AgentTicket::new(key, &addr).encode())
    }

    pub fn agent_keys(&self) -> Result<Vec<crate::mcp_quic::AgentKey>> {
        crate::mcp_quic::list_keys(&self.journal)
    }

    pub fn revoke_agent_key(&self, id: &str) -> Result<()> {
        crate::mcp_quic::revoke_key(&self.journal, id)
    }

    /// Pairing ticket for this journal, pointing at this peer.
    pub fn ticket(&self) -> Result<String> {
        let addr = self.endpoint.addr();
        if addr.addrs.is_empty() {
            bail!("endpoint address not ready yet");
        }
        Ok(JournalTicket::new(*self.journal.secret(), &addr).encode())
    }

    /// Store media bytes in the blob store and append a capture event referencing them.
    /// Audio captured this way is `AudioKind::Voice`; use `capture_audio` for music.
    pub async fn capture_blob(&self, kind: MediaKind, bytes: Vec<u8>) -> Result<Event> {
        self.capture_blob_with_intent(kind, bytes, false).await
    }

    /// `will_enrich` = "this peer intends to enrich this capture itself" — others
    /// hold off for the grace period.
    pub async fn capture_blob_with_intent(
        &self,
        kind: MediaKind,
        bytes: Vec<u8>,
        will_enrich: bool,
    ) -> Result<Event> {
        self.capture_with(kind, bytes, AudioKind::Voice, will_enrich).await
    }

    /// Audio capture with its kind decided up front (retention.rs). Music is
    /// never enriched, so `will_enrich` only applies to voice.
    pub async fn capture_audio(&self, bytes: Vec<u8>, audio_kind: AudioKind) -> Result<Event> {
        self.capture_audio_with_intent(bytes, audio_kind, false).await
    }

    pub async fn capture_audio_with_intent(
        &self,
        bytes: Vec<u8>,
        audio_kind: AudioKind,
        will_enrich: bool,
    ) -> Result<Event> {
        let will_enrich = will_enrich && audio_kind == AudioKind::Voice;
        self.capture_with(MediaKind::Audio, bytes, audio_kind, will_enrich).await
    }

    async fn capture_with(
        &self,
        kind: MediaKind,
        bytes: Vec<u8>,
        audio_kind: AudioKind,
        will_enrich: bool,
    ) -> Result<Event> {
        let (payload, tag) = self.seal_and_store(kind, bytes, audio_kind).await?;
        let event = self.journal.store.append_local(
            self.journal.device_id(),
            crate::event::EventKind::Capture,
            payload,
            will_enrich,
        )?;
        self.drop_ingest_tag(tag).await;
        Ok(event)
    }

    /// Once the capture event exists the log protects the blob; the ingest
    /// tag would otherwise pin it past retention forever. Best effort — a
    /// leftover tag is cleaned up at the next spawn.
    async fn drop_ingest_tag(&self, tag: iroh_blobs::api::tags::TagInfo) {
        if let Err(err) = self.blobs.tags().delete(tag.name).await {
            tracing::debug!("ingest tag not dropped: {err:#}");
        }
    }

    /// Media capture with an explicit `recorded_at` (import tool).
    pub async fn capture_blob_at(
        &self,
        kind: MediaKind,
        bytes: Vec<u8>,
        recorded_at: i64,
    ) -> Result<Event> {
        let (payload, tag) = self.seal_and_store(kind, bytes, AudioKind::Voice).await?;
        let event = self.journal.store.append_local_at(
            self.journal.device_id(),
            crate::event::EventKind::Capture,
            payload,
            false,
            recorded_at,
        )?;
        self.drop_ingest_tag(tag).await;
        Ok(event)
    }

    /// Encrypt-at-ingest: seal plaintext under a fresh content key, store the
    /// ciphertext (the blob identity is the ciphertext hash), wrap the key
    /// into the payload. The blob store never sees plaintext.
    async fn seal_and_store(
        &self,
        kind: MediaKind,
        bytes: Vec<u8>,
        audio_kind: AudioKind,
    ) -> Result<(Payload, iroh_blobs::api::tags::TagInfo)> {
        let size = bytes.len() as u64;
        let mut sealed = tokio::task::spawn_blocking(move || crate::crypto::seal(&bytes)).await??;
        let envelope = self.journal.wrap_blob_keys(&sealed)?;
        let ciphertext = std::mem::take(&mut sealed.ciphertext);
        let tag = self.blobs.add_bytes(ciphertext).await?;
        let payload =
            Payload::media_audio_kind(kind, tag.hash.to_hex().to_string(), size, envelope, audio_kind);
        Ok((payload, tag))
    }

    /// Whole blob plaintext, by (ciphertext) hex hash: fetch, unwrap the
    /// content key from the referencing capture event, decrypt.
    ///
    /// This holds the ciphertext and the plaintext at once. The HTTP media
    /// route serves many blobs together and must use [`Self::open_blob`]
    /// instead, which decrypts one chunk at a time.
    pub async fn blob_bytes(&self, hash_hex: &str) -> Result<Vec<u8>> {
        let hash: Hash = hash_hex.parse().context("bad blob hash")?;
        let (capture_event_id, payload) = self
            .journal
            .store
            .capture_for_hash(hash_hex)?
            .context("no capture references this blob")?;
        let original = payload
            .blob_crypto()
            .context("media predates encryption — run `memorious migrate-encrypt`")?;
        // A master-password rotation since this was captured leaves the
        // original envelope wrapped under the old key forever (events are
        // immutable) — try every envelope this capture has had for the one
        // that matches whatever key *this* device currently has active.
        let (ck, nonce_base) = self.journal.unwrap_capture_blob_keys(&capture_event_id, original)?;
        let ciphertext = self.blobs.get_bytes(hash).await?;
        tokio::task::spawn_blocking(move || crate::crypto::open(&ciphertext, &ck, &nonce_base))
            .await?
    }

    /// Plaintext of one stored blob, decrypted a chunk at a time.
    ///
    /// The chunk size is the seal size (64 KiB plus its tag). Callers that
    /// pull the stream and drop each chunk — the HTTP media handler — keep
    /// a few kilobytes per request instead of a copy of the file.
    pub async fn open_blob(
        &self,
        hash_hex: &str,
    ) -> std::result::Result<OpenedBlob, BlobOpenError> {
        let hash: Hash = hash_hex
            .parse()
            .map_err(|e| BlobOpenError::Unavailable(anyhow::anyhow!("bad blob hash: {e}")))?;
        let (capture_event_id, payload) = self
            .journal
            .store
            .capture_for_hash(hash_hex)
            .map_err(BlobOpenError::Unavailable)?
            .ok_or(BlobOpenError::Unreferenced)?;
        let kind = payload.media_kind().ok_or(BlobOpenError::Unreferenced)?;
        let len = payload.media_len().ok_or(BlobOpenError::Unreferenced)?;
        let original = payload.blob_crypto().ok_or_else(|| {
            BlobOpenError::Unavailable(anyhow::anyhow!(
                "media predates encryption — run `memorious migrate-encrypt`"
            ))
        })?;
        let (ck, nonce_base) = self
            .journal
            .unwrap_capture_blob_keys(&capture_event_id, original)
            .map_err(BlobOpenError::Unavailable)?;
        let n_chunks = len.div_ceil(crate::crypto::CHUNK_LEN as u64).max(1);
        if n_chunks > u32::MAX as u64 {
            return Err(BlobOpenError::Unavailable(anyhow::anyhow!("blob too large")));
        }
        // Refuse before the first plaintext byte if the stored ciphertext is
        // missing or not the length `seal` would have written for `len`.
        let expected = len + n_chunks * crate::crypto::TAG_LEN as u64;
        match self
            .blobs
            .status(hash)
            .await
            .map_err(|e| BlobOpenError::Unavailable(e.into()))?
        {
            iroh_blobs::api::blobs::BlobStatus::Complete { size } if size == expected => {}
            iroh_blobs::api::blobs::BlobStatus::Complete { size } => {
                return Err(BlobOpenError::Unavailable(anyhow::anyhow!(
                    "blob length {size} does not match sealed plaintext {len}"
                )));
            }
            iroh_blobs::api::blobs::BlobStatus::NotFound
            | iroh_blobs::api::blobs::BlobStatus::Partial { .. } => {
                return Err(BlobOpenError::Unavailable(anyhow::anyhow!(
                    "blob not on this device"
                )));
            }
        }
        let n_chunks = n_chunks as u32;
        let ct_len = chunk_ct_len(0, n_chunks, len);
        Ok(OpenedBlob {
            kind,
            len,
            plaintext: BlobPlaintext {
                reader: Box::pin(self.blobs.reader(hash)),
                ck,
                nonce_base,
                index: 0,
                n_chunks,
                plaintext_len: len,
                buf: vec![0u8; ct_len],
                filled: 0,
                ct_len,
                done: false,
                failed: false,
            },
        })
    }

    pub async fn has_blob(&self, hash_hex: &str) -> Result<bool> {
        let hash: Hash = hash_hex.parse().context("bad blob hash")?;
        Ok(self.blobs.has(hash).await?)
    }

    /// This device's media tally — what it tells peers in the handshake and
    /// what the status screen shows for "this device".
    pub async fn media_held(&self) -> Result<MediaHeld> {
        media_held(&self.blobs, &self.journal).await
    }

    /// Referenced blobs this device does not hold and is still waiting for:
    /// not let go by retention, not declared lost. Each row names the capture
    /// that references it, so a human can decide whether it is `media_lost`.
    pub async fn missing_media(&self) -> Result<Vec<serde_json::Value>> {
        let policy = self.journal.retention_policy()?;
        let evictable: HashSet<String> = self
            .journal
            .evictable_blob_hashes(&policy, unix_now_ms())?
            .into_iter()
            .collect();
        let lost = self.journal.lost_blob_hashes()?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for hex in self.journal.store.referenced_blob_hashes()? {
            if evictable.contains(&hex) || lost.contains(&hex) || !seen.insert(hex.clone()) {
                continue;
            }
            let hash: Hash = hex.parse().context("bad hash in log")?;
            if self.blobs.has(hash).await? {
                continue;
            }
            let capture = self.journal.store.capture_for_hash(&hex)?;
            let (event_id, kind) = match &capture {
                Some((id, p)) => (Some(id.clone()), p.media_kind().map(|k| format!("{k:?}").to_lowercase())),
                None => (None, None),
            };
            let recorded_at = match &event_id {
                Some(id) => self.journal.store.get_event(id)?.map(|e| e.recorded_at),
                None => None,
            };
            out.push(serde_json::json!({"hash": hex, "capture": event_id, "kind": kind, "recorded_at": recorded_at}));
        }
        Ok(out)
    }

    /// One full sync round-trip with the peer at `addr`: events, then media.
    pub async fn sync_with(&self, addr: &EndpointAddr) -> Result<SyncReport> {
        if self.journal.is_isolated()? {
            anyhow::bail!("this peer is isolated: end isolation before syncing");
        }
        let mut report = self.sync_events_with(addr).await?;
        if self.journal.is_write_only()? {
            return Ok(report); // nothing to fetch: a write-only peer references no one else's media
        }
        report.blobs_fetched = fetch_missing_blobs(
            &self.endpoint,
            &self.blobs,
            &self.journal,
            addr,
            &self.fetch_gate,
        )
        .await
        .context("fetch blobs")?;
        Ok(report)
    }

    /// Event-log-only round-trip: timelines converge, media stays deferred. Records the attempt (in flight, then how it went)
    /// for the Peers page.
    pub async fn sync_events_with(&self, addr: &EndpointAddr) -> Result<SyncReport> {
        let guard = AttemptGuard::begin(&self.sync_state, addr.id.to_string());
        let result = self.sync_events_with_inner(addr).await;
        guard.finish(result.as_ref().err().map(|e| format!("{e:#}")));
        result
    }

    async fn sync_events_with_inner(&self, addr: &EndpointAddr) -> Result<SyncReport> {
        if self.journal.is_isolated()? {
            anyhow::bail!("this peer is isolated: end isolation before syncing");
        }
        let conn = tokio::time::timeout(SYNC_CONNECT_TIMEOUT, self.endpoint.connect(addr.clone(), SYNC_ALPN))
            .await
            .map_err(|_| {
                anyhow::anyhow!("peer unreachable ({}s)", SYNC_CONNECT_TIMEOUT.as_secs())
            })?
            .context("connect to peer")?;
        let (mut send, mut recv) = conn.open_bi().await?;

        let my_addr = self.dialable_addr().await.unwrap_or_else(|_| self.endpoint.addr());
        let _ = self.journal.touch_alive(unix_now_ms());
        write_msg(
            &mut send,
            &Msg::Hello {
                auth: auth_token(self.journal.secret()),
                heads: self.journal.store.heads()?,
                addr: AddrWire::from_addr(&my_addr),
                device: Some(self.journal.device_id().to_string()),
                media: self.media_held().await.ok(),
                receive: self.journal.is_write_only()?.then_some(false),
                alive: self.journal.alive_self().ok(),
            },
        )
        .await?;

        let first = read_msg(&mut recv).await;
        let (peer_heads, peer_device, peer_media, peer_receive, peer_alive) = match first {
            Ok(Some(Msg::HelloAck { heads, device, media, receive, alive, .. })) => (heads, device, media, receive, alive),
            other => {
                // the close code may land a moment after the stream ends
                let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                if knocked_back(&conn) {
                    return Err(Knocking { code: crate::trust::knock_code(&self.endpoint.id().to_string()) }.into());
                }
                match other {
                    Ok(Some(other)) => bail!("protocol violation: expected HelloAck, got {other:?}"),
                    Ok(None) => bail!("peer closed during handshake (bad journal secret?)"),
                    Err(e) => return Err(e),
                }
            }
        };

        // Receive what we're missing.
        let write_only = self.journal.is_write_only()?;
        let mut report = SyncReport::default();
        loop {
            match read_msg(&mut recv).await? {
                // a write-only peer takes nothing in, even from a peer that sends anyway
                Some(Msg::Event(_)) if write_only => {}
                Some(Msg::Event(ev)) => {
                    if self.journal.store.insert_remote(&ev)? {
                        report.received += 1;
                    }
                }
                Some(Msg::EndEvents) => break,
                Some(other) => bail!("protocol violation: expected Event/EndEvents, got {other:?}"),
                None => bail!("peer closed mid event stream"),
            }
        }

        // Send what the peer is missing.
        let to_send = if peer_receive == Some(false) { Vec::new() } else { self.journal.store.events_missing_from(&peer_heads)? };
        for ev in to_send {
            write_msg(&mut send, &Msg::Event(ev)).await?;
            report.sent += 1;
        }
        write_msg(&mut send, &Msg::EndEvents).await?;
        send.finish()?;

        match read_msg(&mut recv).await? {
            Some(Msg::Done) => {}
            other => bail!("protocol violation: expected Done, got {other:?}"),
        }

        conn.close(VarInt::from(0u32), b"done");
        // We dial only addresses that came out of a pairing ticket (fresh or
        // remembered) — that's how this peer was discovered.
        // we chose to dial it and it answered: it is one of ours
        self.journal.allow_peer(&addr.id.to_string())?;
        self.journal.record_sync_contact(
            &addr.id.to_string(),
            unix_now_ms(),
            peer_device.as_deref(),
            "ticket",
        )?;
        let _ = self.journal.note_link(&addr.id.to_string(), true, unix_now_ms());
        if let Some(alive) = &peer_alive {
            let _ = self.journal.record_peer_alive(&addr.id.to_string(), alive, unix_now_ms());
        }
        // Converged: the peer now holds exactly our heads.
        self.journal.record_peer_holdings(
            &addr.id.to_string(),
            &self.journal.store.heads()?,
            peer_media.as_ref(),
        )?;
        self.remember_peer_addr(addr);
        Ok(report)
    }

    /// The owner tapped accept on a knock: let that endpoint in. Its next sync (it keeps knocking) goes through.
    pub fn accept_knock(&self, endpoint_id: &str) -> Result<()> {
        self.journal.allow_peer(endpoint_id)
    }

    pub fn reject_knock(&self, endpoint_id: &str) -> Result<()> {
        self.journal.reject_knock(endpoint_id)
    }

    /// Remove a peer: forget it entirely, so it stops being dialled and, if it syncs with us again, it is a first knock.
    pub fn remove_peer(&self, endpoint_id: &str) -> Result<()> {
        self.journal.remove_peer(endpoint_id)?;
        self.sync_state.attempts.lock().unwrap().remove(endpoint_id);
        Ok(())
    }

    /// Known sync peers with their device mapping, last contact, and (when
    /// the endpoint still holds a live path) the transport in use.
    pub async fn peers(&self) -> Result<Vec<PeerInfo>> {
        let store = &self.journal.store;
        let mut out = Vec::new();
        for (key, value) in store.meta_scan("peer_last_ok:")? {
            let endpoint_id = key.trim_start_matches("peer_last_ok:").to_string();
            let endpoint_id_for_required = endpoint_id.clone();
            let last_ok_ms = value
                .as_slice()
                .try_into()
                .map(i64::from_le_bytes)
                .unwrap_or(0);
            let meta_str = |prefix: &str| -> Option<String> {
                store
                    .meta_get(&format!("{prefix}{endpoint_id}"))
                    .ok()
                    .flatten()
                    .and_then(|b| String::from_utf8(b).ok())
            };
            // Of the currently-active paths, prefer a direct one — iroh moves
            // data off the relay as soon as a direct path works, so an active
            // direct path means the relay (if also active) is only standby.
            let conn = match endpoint_id.parse::<iroh::EndpointId>() {
                Ok(id) => self.endpoint.remote_info(id).await.and_then(|info| {
                    let active: Vec<_> = info
                        .addrs()
                        .filter(|a| matches!(a.usage(), iroh::endpoint::TransportAddrUsage::Active))
                        .map(|a| a.addr().clone())
                        .collect();
                    active
                        .iter()
                        .find_map(|a| match a {
                            iroh::TransportAddr::Ip(sock) => Some(PeerConn {
                                transport: "direct".into(),
                                detail: sock.to_string(),
                                lan: is_lan_ip(&sock.ip()),
                                proxied: false,
                            }),
                            _ => None,
                        })
                        .or_else(|| {
                            active.iter().find_map(|a| match a {
                                iroh::TransportAddr::Relay(url) => Some(PeerConn {
                                    transport: "relay".into(),
                                    detail: url.to_string(),
                                    lan: false,
                                    proxied: true,
                                }),
                                _ => None,
                            })
                        })
                }),
                Err(_) => None,
            };
            let device_id = meta_str("peer_device:");
            let discovery = meta_str("peer_discovery:");
            let holdings = self.journal.peer_holdings(&endpoint_id)?;
            let events_held = holdings.heads.as_ref().map(|h| h.values().sum());
            let events_missing = match &holdings.heads {
                Some(h) => Some(self.journal.peer_events_missing(h)?),
                None => None,
            };
            let media_missing = holdings.media.as_ref().map(MediaHeld::missing);
            let newest_held = match &holdings.heads {
                Some(h) => self.journal.newest_event(Some(h))?,
                None => None,
            };
            let attempt = self.sync_state.attempts.lock().unwrap().get(&endpoint_id).cloned().unwrap_or_default();
            out.push(PeerInfo {
                endpoint_id,
                device_id,
                last_ok_ms,
                discovery,
                conn,
                heads: holdings.heads,
                events_held,
                events_missing,
                media: holdings.media,
                media_missing,
                required: self.journal.peer_required(&endpoint_id_for_required)?,
                newest_held,
                syncing: attempt.in_flight > 0,
                last_attempt_ms: attempt.last_attempt_ms,
                last_error: attempt.last_error,
            });
        }
        out.sort_by_key(|p| std::cmp::Reverse(p.last_ok_ms));
        Ok(out)
    }

    /// The Peers page's 7-day history for every known peer (see `alive.rs`): one shape for every face.
    pub async fn peers_history(&self) -> Result<serde_json::Value> {
        let now = unix_now_ms();
        let mut peers = Vec::new();
        for p in self.peers().await? {
            peers.push(self.journal.peer_history(&p.endpoint_id, now)?);
        }
        Ok(serde_json::json!({"now": now, "slot_ms": crate::alive::SLOT_MS, "peers": peers}))
    }

    /// The status screen's JSON, one shape for every face (HTTP, Tauri, FFI).
    pub async fn status_json(&self) -> Result<serde_json::Value> {
        let journal = self.journal();
        let timeline = journal.timeline()?;
        // Oldest join per device — a device that (implausibly) authored more
        // than one keeps its earliest, matching "when it first joined".
        let mut joins = std::collections::HashMap::new();
        for (device_id, recorded_at) in journal.peer_joins()? {
            joins.entry(device_id).or_insert(recorded_at);
        }
        let sync = {
            let sc = *self.sync_state.schedule.lock().unwrap();
            serde_json::json!({"next_ms": sc.next_ms, "interval_ms": sc.interval_ms, "nudge_at_ms": sc.nudge_at_ms})
        };
        let mut v = serde_json::json!({
            "device_id": journal.device_id(),
            "isolated": journal.is_isolated()?,
            "write_only": journal.is_write_only()?,
            "knocks": journal.knocks(unix_now_ms())?,
            "knock_code": crate::trust::knock_code(&self.endpoint.id().to_string()),
            "retention": journal.retention_policy()?,
            "entries": timeline.entries,
            "trash": journal.trash()?.len(),
            "heads": journal.store.heads()?,
            "events_total": journal.events_total()?,
            "media": self.media_held().await?,
            "timeline": timeline,
            "storage": journal.storage_usage()?,
            "health": journal.sync_health(unix_now_ms())?,
            "names": journal.device_names()?,
            "joins": joins,
            "peers": self.peers().await?,
            "newest_here": journal.newest_event(None)?,
            "sync": sync,
            "net": journal.net_config(),
            "version": crate::VERSION,
            "git_sha": crate::GIT_SHA,
            "build_date": crate::BUILD_DATE,
            // device_id → that device's own announced build (incl. this one) —
            // the Peers page's per-row version/sha/date and "behind" marker.
            "builds": journal.peer_builds()?,
            "newer_version": journal.newer_version_available()?,
        });
        if let Ok(t) = self.ticket() {
            v["ticket"] = t.into();
        }
        Ok(v)
    }

    pub async fn shutdown(self) {
        self.shutdown_ref().await;
    }

    /// Same as `shutdown`, for callers that only hold an `Arc<Node>` (the
    /// mobile face: the app closes the journal before deleting its data
    /// dir). Releases the endpoint, the blob store and its database.
    pub async fn shutdown_ref(&self) {
        let _ = self.router.shutdown().await;
        let _ = self.blobs.shutdown().await;
        self.endpoint.close().await;
    }
}

/// Pull every blob referenced by the log that we don't hold, from `provider`.
/// Count referenced blobs against the store. One `has` per referenced hash:
/// local lookups, cheap enough for every handshake.
async fn media_held(blobs: &BlobStore, journal: &Journal) -> Result<MediaHeld> {
    let policy = journal.retention_policy()?;
    let evictable: HashSet<String> = journal
        .evictable_blob_hashes(&policy, unix_now_ms())?
        .into_iter()
        .collect();
    let mut tally = MediaHeld {
        policy: Some(policy),
        bytes: journal.storage_usage()?.blobs_bytes,
        ..MediaHeld::default()
    };
    let lost = journal.lost_blob_hashes()?;
    for hex in journal.store.referenced_blob_hashes()? {
        tally.referenced += 1;
        let hash: Hash = hex.parse().context("bad hash in log")?;
        if blobs.has(hash).await? {
            tally.held += 1;
        } else if lost.contains(&hex) {
            tally.lost += 1;
        } else if evictable.contains(&hex) {
            tally.evictable += 1;
        }
    }
    Ok(tally)
}

/// First retry after a blob fetch fails, then doubling until [`BLOB_RETRY_CAP`].
const BLOB_RETRY_BASE: Duration = Duration::from_secs(15);
/// Stop lengthening the pause. A blob the peer cannot serve is asked again
/// every fifteen minutes, not on every handshake.
const BLOB_RETRY_CAP: Duration = Duration::from_secs(15 * 60);

struct BlobRetry {
    failures: u32,
    not_before: Instant,
}

/// Serialises blob downloads and remembers hashes a peer just failed to serve.
///
/// Every inbound sync calls [`fetch_missing_blobs`], and the initiator closes
/// its sync stream before that download finishes, so a peer that reconnects
/// in a loop used to open one QUIC connection per attempt. A failed attempt
/// does not drop that connection immediately (it drains until the idle
/// timeout). Measured on a reset-at-header peer — the production failure
/// `io: stream reset by peer: error 3`, iroh-blobs `ERR_INTERNAL` — forty
/// sequential attempts retained about 7MB and a few hundred plateaued near
/// 14MB, with overlapping attempts stacking on top. One download at a time,
/// and a pause after a hash that actually failed, keeps a reconnect storm
/// from allocating a connection per handshake.
struct FetchGate {
    slots: tokio::sync::Semaphore,
    retries: Mutex<HashMap<Hash, BlobRetry>>,
}

impl FetchGate {
    fn new() -> Self {
        Self {
            slots: tokio::sync::Semaphore::new(1),
            retries: Mutex::new(HashMap::new()),
        }
    }

    fn retry_delay(failures: u32) -> Duration {
        let exp = failures.saturating_sub(1).min(6);
        let secs = BLOB_RETRY_BASE
            .as_secs()
            .checked_shl(exp)
            .unwrap_or(u64::MAX)
            .min(BLOB_RETRY_CAP.as_secs());
        Duration::from_secs(secs)
    }

    fn ready(&self, hash: &Hash) -> bool {
        let map = self.retries.lock().unwrap();
        match map.get(hash) {
            Some(retry) => Instant::now() >= retry.not_before,
            None => true,
        }
    }

    fn note_failure(&self, hash: Hash) {
        let mut map = self.retries.lock().unwrap();
        let entry = map.entry(hash).or_insert(BlobRetry {
            failures: 0,
            not_before: Instant::now(),
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.not_before = Instant::now() + Self::retry_delay(entry.failures);
    }

    fn note_success(&self, hash: &Hash) {
        self.retries.lock().unwrap().remove(hash);
    }

    #[cfg(test)]
    fn clear(&self) {
        self.retries.lock().unwrap().clear();
    }
}

async fn fetch_missing_blobs(
    endpoint: &Endpoint,
    blobs: &BlobStore,
    journal: &Journal,
    provider: &EndpointAddr,
    gate: &FetchGate,
) -> Result<usize> {
    // A burst of handshakes must not each open a download. The one in flight
    // covers this peer; the next handshake retries anything it skipped.
    let Ok(_permit) = gate.slots.try_acquire() else {
        tracing::debug!("blob fetch already in progress");
        return Ok(0);
    };

    // Retention: don't pull back what this device's policy says it may drop
    // (retention.rs) — the GC would only remove it again.
    let evictable: HashSet<String> = journal
        .evictable_blob_hashes(&journal.retention_policy()?, unix_now_ms())?
        .into_iter()
        .collect();
    let lost = journal.lost_blob_hashes()?;
    let mut missing = Vec::new();
    let mut seen = HashSet::new();
    for hex in journal.store.referenced_blob_hashes()? {
        if evictable.contains(&hex) || lost.contains(&hex) {
            continue;
        }
        let hash: Hash = hex.parse().context("bad hash in log")?;
        if !seen.insert(hash) {
            continue;
        }
        if !gate.ready(&hash) {
            tracing::debug!("blob {} still in retry backoff", hash.fmt_short());
            continue;
        }
        if !blobs.has(hash).await? {
            missing.push(hash);
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    let conn = tokio::time::timeout(
        SYNC_CONNECT_TIMEOUT,
        endpoint.connect(provider.clone(), iroh_blobs::ALPN),
    )
    .await
    .map_err(|_| anyhow::anyhow!("peer unreachable ({}s)", SYNC_CONNECT_TIMEOUT.as_secs()))?
    .context("connect for blobs")?;
    let mut fetched = 0;
    for hash in missing {
        match blobs.remote().fetch(conn.clone(), hash).await {
            Ok(_) => {
                gate.note_success(&hash);
                fetched += 1;
            }
            // The provider may legitimately not hold this blob (it came from a third
            // peer). Sync must not fail over it; a later sync will heal it — after
            // the retry pause, so a hash that resets on every attempt cannot pin a
            // new connection per handshake.
            Err(err) => {
                gate.note_failure(hash);
                tracing::warn!("blob {} not fetched: {err:#}", hash.fmt_short());
            }
        }
    }
    conn.close(VarInt::from(0u32), b"done");
    Ok(fetched)
}

/// One peer's answer to "can I reach you right now?".
#[derive(Debug, Clone, Serialize)]
pub struct PeerPing {
    pub endpoint_id: String,
    pub device_id: Option<String>,
    pub ok: bool,
    /// Handshake-to-Done round trip, when reachable.
    pub rtt_ms: Option<u64>,
    pub error: Option<String>,
}

impl Node {
    /// Remember the address a peer was actually reached at (latest wins), so
    /// it can be probed later without a fresh ticket. Best effort — meta only.
    fn remember_peer_addr(&self, addr: &EndpointAddr) {
        if let Ok(json) = serde_json::to_vec(&AddrWire::from_addr(addr)) {
            let _ = self
                .journal
                .store
                .meta_set(&format!("peer_addr:{}", addr.id), &json);
        }
    }

    fn stored_peer_addr(&self, endpoint_id: &str) -> Result<Option<EndpointAddr>> {
        match self.journal.store.meta_get(&format!("peer_addr:{endpoint_id}"))? {
            Some(bytes) => {
                let wire: AddrWire = serde_json::from_slice(&bytes)?;
                Ok(Some(wire.to_addr()?))
            }
            None => Ok(None),
        }
    }

    /// Push captures to known peers without holding up the capturing face.
    /// Offline peers keep their missing events until the next ordinary sync.
    pub async fn sync_known_peers(&self) -> Result<()> {
        for outcome in self.sync_saved_peers().await? {
            if let Some(error) = outcome.error {
                tracing::debug!("capture sync deferred: {error}");
            }
        }
        Ok(())
    }

    /// Sync every known peer concurrently, including peers first met inbound.
    /// An offline peer must not prevent the remaining peers from converging.
    pub async fn sync_saved_peers(&self) -> Result<Vec<PeerSync>> {
        let peers = self.peers().await?;
        Ok(n0_future::join_all(peers.into_iter().map(|peer| async move {
            let result: Result<SyncReport> = async {
                let addr = match self.stored_peer_addr(&peer.endpoint_id)? {
                    Some(addr) => addr,
                    None => EndpointAddr::new(peer.endpoint_id.parse()?),
                };
                self.sync_with(&addr).await
            }.await;
            match result {
                Ok(report) => PeerSync { endpoint_id: peer.endpoint_id, ok: true,
                    sent: report.sent, received: report.received, blobs: report.blobs_fetched, error: None },
                Err(error) => PeerSync { endpoint_id: { let _ = self.journal.note_link(&peer.endpoint_id, false, unix_now_ms()); peer.endpoint_id }, ok: false,
                    sent: 0, received: 0, blobs: 0, error: Some(format!("{error:#}")) },
            }
        })).await)
    }

    /// Probe one known peer: the ordinary event sync with a stopwatch and a
    /// timeout. Reachable literally means "able to sync", and a behind peer is
    /// healed by the probe. Never errors — the answer *is* the outcome.
    pub async fn ping_peer(&self, endpoint_id: &str, device_id: Option<String>, timeout: Duration) -> PeerPing {
        let mut ping = PeerPing {
            endpoint_id: endpoint_id.to_string(),
            device_id,
            ok: false,
            rtt_ms: None,
            error: None,
        };
        let addr = match self.stored_peer_addr(endpoint_id) {
            Ok(Some(addr)) => addr,
            Ok(None) => {
                // Synced before this build (or never): no address on file yet;
                // the next real sync records one.
                ping.error = Some("no known address for this peer yet".into());
                return ping;
            }
            Err(err) => {
                ping.error = Some(format!("{err:#}"));
                return ping;
            }
        };
        let start = std::time::Instant::now();
        match tokio::time::timeout(timeout, self.sync_events_with(&addr)).await {
            Ok(Ok(_)) => {
                ping.ok = true;
                ping.rtt_ms = Some(start.elapsed().as_millis() as u64);
            }
            Ok(Err(err)) => ping.error = Some(format!("{err:#}")),
            Err(_) => ping.error = Some(format!("no answer within {}s", timeout.as_secs())),
        }
        if !ping.ok {
            let _ = self.journal.note_link(endpoint_id, false, unix_now_ms());
        }
        ping
    }

    /// Probe every known peer concurrently. Order matches `peers()`
    /// (most recently heard from first).
    pub async fn ping_peers(&self, timeout: Duration) -> Result<Vec<PeerPing>> {
        if self.journal.is_isolated()? {
            return Ok(Vec::new());
        }
        let peers = self.peers().await?;
        let probes = peers
            .into_iter()
            .map(|p| self.ping_peer_owned(p.endpoint_id, p.device_id, timeout));
        Ok(n0_future::join_all(probes).await)
    }

    async fn ping_peer_owned(&self, endpoint_id: String, device_id: Option<String>, timeout: Duration) -> PeerPing {
        self.ping_peer(&endpoint_id, device_id, timeout).await
    }
}

impl Drop for Node {
    /// The blob store's GC task (retention) holds its own handle to the store
    /// actor, so merely dropping `FsStore` no longer closes the database — a
    /// Node dropped without `shutdown()` would keep `blobs.db` locked for the
    /// life of the process. Ask the actor to stop explicitly (idempotent after
    /// `shutdown`; best effort outside a runtime).
    fn drop(&mut self) {
        let blobs = self.blobs.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = blobs.shutdown().await;
            });
        }
    }
}

/// Hashes the GC must keep: everything the log references minus what this
/// device's retention policy lets go. The policy itself is `retention.rs`.
fn live_blob_hashes(journal: &Journal) -> Result<Vec<Hash>> {
    let evictable: HashSet<String> = journal
        .evictable_blob_hashes(&journal.retention_policy()?, unix_now_ms())?
        .into_iter()
        .collect();
    let mut live = Vec::new();
    for hex in journal.store.referenced_blob_hashes()? {
        if evictable.contains(&hex) {
            continue;
        }
        live.push(hex.parse::<Hash>().context("bad hash in log")?);
    }
    Ok(live)
}

/// Remove ingest tags for blobs the log already references, so the log is the
/// only GC root (older journals tagged every blob at ingest). Tags on
/// *unreferenced* blobs are left alone: they belong to captures still in
/// flight between ingest and event append.
async fn drop_ingest_tags_for_referenced(blobs: &BlobStore, journal: &Journal) -> Result<()> {
    use n0_future::StreamExt;
    let referenced: HashSet<Hash> = journal
        .store
        .referenced_blob_hashes()?
        .iter()
        .filter_map(|h| h.parse().ok())
        .collect();
    let mut tags = blobs.tags().list().await?;
    let mut names = Vec::new();
    while let Some(tag) = tags.next().await {
        let tag = tag?;
        if referenced.contains(&tag.hash) {
            names.push(tag.name);
        }
    }
    for name in names {
        blobs.tags().delete(name).await?;
    }
    Ok(())
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// anyhow::Error doesn't impl std::error::Error; box it for AcceptError.
fn acc(err: anyhow::Error) -> AcceptError {
    AcceptError::from_boxed(err.into())
}

#[derive(Clone)]
struct SyncProto {
    journal: Arc<Journal>,
    blobs: BlobStore,
    endpoint: Endpoint,
    fetch_gate: Arc<FetchGate>,
}

impl std::fmt::Debug for SyncProto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncProto").finish_non_exhaustive()
    }
}

/// Wraps a protocol so an isolated peer refuses its connections (see `isolate.rs`).
#[derive(Clone)]
struct Gated<P> {
    inner: P,
    journal: Arc<Journal>,
}

impl<P> std::fmt::Debug for Gated<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gated").finish_non_exhaustive()
    }
}

impl<P: ProtocolHandler> ProtocolHandler for Gated<P> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if self.journal.is_isolated().unwrap_or(false) {
            connection.close(VarInt::from(CLOSE_ISOLATED), b"isolated");
            return Err(AcceptError::from_err(std::io::Error::other("this peer is isolated")));
        }
        // media is for peers that are in, same as events
        if !self.journal.is_peer_allowed(&connection.remote_id().to_string()).unwrap_or(false) {
            connection.close(VarInt::from(CLOSE_KNOCKING), b"knocking");
            return Err(AcceptError::from_err(std::io::Error::other("this peer is not allowed in")));
        }
        self.inner.accept(connection).await
    }
}

impl ProtocolHandler for SyncProto {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if self.journal.is_isolated().unwrap_or(false) {
            connection.close(VarInt::from(CLOSE_ISOLATED), b"isolated");
            return Err(AcceptError::from_err(std::io::Error::other("this peer is isolated")));
        }
        let (mut send, mut recv) = connection.accept_bi().await?;

        let (heads, initiator_addr, initiator_device, initiator_media, initiator_receive, initiator_alive) =
            match read_msg(&mut recv).await.map_err(acc)? {
                Some(Msg::Hello { auth, heads, addr, device, media, receive, alive, .. }) => {
                    if auth != auth_token(self.journal.secret()) {
                        connection.close(VarInt::from(CLOSE_BAD_AUTH), b"bad auth");
                        return Err(AcceptError::from_err(std::io::Error::other(
                            "peer failed journal auth",
                        )));
                    }
                    (heads, addr.to_addr().map_err(acc)?, device, media, receive, alive)
                }
                _ => {
                    return Err(AcceptError::from_err(std::io::Error::other(
                        "protocol violation: expected Hello",
                    )))
                }
            };

        // The secret got this peer to the door; whether it comes in is the owner's call (trust.rs). A stranger is recorded as a
        // knock and told nothing: no heads, no events, no media tally.
        let remote = connection.remote_id().to_string();
        if !self.journal.is_peer_allowed(&remote).map_err(acc)? {
            let _ = self.journal.record_knock(&remote, initiator_device.as_deref(), unix_now_ms());
            connection.close(VarInt::from(CLOSE_KNOCKING), b"knocking");
            return Ok(());
        }

        let my_heads = self.journal.store.heads().map_err(acc)?;
        let my_media = media_held(&self.blobs, &self.journal).await.ok();
        write_msg(
            &mut send,
            &Msg::HelloAck {
                heads: my_heads,
                device: Some(self.journal.device_id().to_string()),
                media: my_media,
                receive: self.journal.is_write_only().map_err(acc)?.then_some(false),
                alive: { let _ = self.journal.touch_alive(unix_now_ms()); self.journal.alive_self().ok() },
            },
        )
        .await
        .map_err(acc)?;
        let i_am_write_only = self.journal.is_write_only().map_err(acc)?;

        // Send the initiator what it's missing.
        let to_send = if initiator_receive == Some(false) {
            Vec::new() // the initiator is write-only
        } else {
            self.journal.store.events_missing_from(&heads).map_err(acc)?
        };
        for ev in to_send {
            write_msg(&mut send, &Msg::Event(ev))
                .await
                .map_err(acc)?;
        }
        write_msg(&mut send, &Msg::EndEvents)
            .await
            .map_err(acc)?;

        // Receive what we're missing.
        loop {
            match read_msg(&mut recv).await.map_err(acc)? {
                Some(Msg::Event(_)) if i_am_write_only => {} // a write-only peer takes nothing in
                Some(Msg::Event(ev)) => {
                    self.journal.store.insert_remote(&ev).map_err(acc)?;
                }
                Some(Msg::EndEvents) => break,
                _ => {
                    return Err(AcceptError::from_err(std::io::Error::other(
                        "protocol violation: expected Event/EndEvents",
                    )))
                }
            }
        }

        write_msg(&mut send, &Msg::Done)
            .await
            .map_err(acc)?;
        send.finish()?;

        // Events are converged both ways at this point — a real contact.
        let _ = self.journal.record_sync_contact(
            &connection.remote_id().to_string(),
            unix_now_ms(),
            initiator_device.as_deref(),
            "inbound",
        );
        let _ = self.journal.note_link(&connection.remote_id().to_string(), true, unix_now_ms());
        if let Some(alive) = &initiator_alive {
            let _ = self.journal.record_peer_alive(&connection.remote_id().to_string(), alive, unix_now_ms());
        }
        if let Ok(heads) = self.journal.store.heads() {
            let _ = self.journal.record_peer_holdings(
                &connection.remote_id().to_string(),
                &heads,
                initiator_media.as_ref(),
            );
        }
        if let Ok(json) = serde_json::to_vec(&AddrWire::from_addr(&initiator_addr)) {
            let _ = self
                .journal
                .store
                .meta_set(&format!("peer_addr:{}", connection.remote_id()), &json);
        }

        // Pull blobs we now reference but don't hold, dialing back the initiator.
        if let Err(err) = fetch_missing_blobs(
            &self.endpoint,
            &self.blobs,
            &self.journal,
            &initiator_addr,
            &self.fetch_gate,
        )
        .await
        {
            tracing::warn!("responder blob fetch failed: {err:#}");
        }

        connection.closed().await;
        Ok(())
    }
}

// Wire frames are JSON: `Payload` is internally tagged (`#[serde(tag = "type")]`),
// which postcard cannot decode (needs deserialize_any).
async fn write_msg(send: &mut SendStream, msg: &Msg) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    send.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

/// Read one frame; `None` on clean end-of-stream.
async fn read_msg(recv: &mut RecvStream) -> Result<Option<Msg>> {
    use iroh::endpoint::ReadExactError;
    let mut len_bytes = [0u8; 4];
    match recv.read_exact(&mut len_bytes).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.context("frame body")?;
    Ok(Some(serde_json::from_slice(&buf).context("frame decode")?))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    #[test]
    fn ticket_round_trips() {
        let addr = EndpointAddr::new(SecretKey::generate().public())
            .with_ip_addr("127.0.0.1:4242".parse().unwrap());
        let ticket = JournalTicket::new([7u8; SECRET_LEN], &addr);
        let s = ticket.encode();
        assert!(s.starts_with("memorious"));
        let back = JournalTicket::decode(&s).unwrap();
        assert_eq!(back.secret, ticket.secret);
        let back_addr = back.addr().unwrap();
        assert_eq!(back_addr.id, addr.id);
        assert_eq!(back_addr.ip_addrs().count(), 1);
    }

    #[test]
    fn hello_without_device_field_still_decodes() {
        // Wire compat: peers running the previous build send Hello/HelloAck
        // without `device`. JSON frames must keep decoding both ways.
        let old_hello = serde_json::json!({
            "Hello": {
                "auth": vec![0u8; 32],
                "heads": crate::store::Heads::new(),
                "addr": { "id_hex": "00", "relays": [], "ips": [] },
            }
        });
        let msg: Msg = serde_json::from_value(old_hello).unwrap();
        assert!(matches!(msg, Msg::Hello { device: None, media: None, .. }));
        let old_ack = serde_json::json!({ "HelloAck": { "heads": crate::store::Heads::new() } });
        let msg: Msg = serde_json::from_value(old_ack).unwrap();
        assert!(matches!(msg, Msg::HelloAck { device: None, media: None, .. }));
    }

    #[test]
    fn net_config_defaults_validates_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let j = crate::Journal::init(&dir.path().join("j"), "pw").unwrap();

        // Nothing stored yet: the n0 defaults, public lookup on.
        let cfg = j.net_config();
        assert_eq!(cfg.relay_mode, "default");
        assert!(cfg.public_lookup);

        // Custom relays must parse as URLs; garbage is refused.
        let bad = NetConfig {
            relay_mode: "custom".into(),
            relay_urls: vec!["not a url".into()],
            public_lookup: true,
        };
        assert!(j.set_net_config(&bad).is_err());
        let empty_custom = NetConfig {
            relay_mode: "custom".into(),
            relay_urls: vec![],
            public_lookup: true,
        };
        assert!(j.set_net_config(&empty_custom).is_err());
        let unknown_mode = NetConfig {
            relay_mode: "turbo".into(),
            relay_urls: vec![],
            public_lookup: true,
        };
        assert!(j.set_net_config(&unknown_mode).is_err());

        let good = NetConfig {
            relay_mode: "custom".into(),
            relay_urls: vec!["https://relay.example.com".into()],
            public_lookup: false,
        };
        j.set_net_config(&good).unwrap();
        assert_eq!(j.net_config(), good);
    }

    #[test]
    fn lan_classification() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.42", "172.16.9.9", "169.254.1.1", "::1", "fe80::1", "fd00::1"] {
            assert!(is_lan_ip(&ip.parse().unwrap()), "{ip} should be LAN");
        }
        for ip in ["8.8.8.8", "196.25.1.1", "2001:4860::1"] {
            assert!(!is_lan_ip(&ip.parse().unwrap()), "{ip} should not be LAN");
        }
    }

    #[test]
    fn auth_token_is_keyed_and_deterministic() {
        let a = auth_token(&[1u8; SECRET_LEN]);
        let b = auth_token(&[1u8; SECRET_LEN]);
        let c = auth_token(&[2u8; SECRET_LEN]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        // and it is not just the raw secret or a plain hash of it
        assert_ne!(a[..], blake3::hash(&[1u8; SECRET_LEN]).as_bytes()[..]);
    }

    /// Provider that accepts the blobs ALPN and resets the response stream with
    /// error 3 (iroh-blobs `ERR_INTERNAL`) once the request arrives — the same
    /// failure production logs as `io: stream reset by peer: error 3`.
    #[derive(Debug)]
    struct ResetBlobs {
        dials: Arc<AtomicUsize>,
        streams: Arc<AtomicUsize>,
        inflight: Arc<AtomicUsize>,
        max_inflight: Arc<AtomicUsize>,
        hold: Duration,
    }

    impl ProtocolHandler for ResetBlobs {
        async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, Ordering::SeqCst);
            // One connection carries every hash of that fetch. Count streams,
            // not dials, so a duplicate hash is visible.
            loop {
                let (mut send, mut recv) = match connection.accept_bi().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                self.streams.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 256];
                let _ = recv.read(&mut buf).await;
                if !self.hold.is_zero() {
                    tokio::time::sleep(self.hold).await;
                }
                let _ = send.reset(VarInt::from_u32(3));
            }
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn wait_addr(endpoint: &Endpoint) -> EndpointAddr {
        for _ in 0..100 {
            let addr = endpoint.addr();
            if !addr.addrs.is_empty() {
                return addr;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("endpoint never became dialable");
    }

    async fn quiet_node(root: &std::path::Path) -> Node {
        let journal = crate::Journal::init(root, "pw").unwrap();
        journal
            .set_net_config(&NetConfig {
                relay_mode: "disabled".into(),
                relay_urls: vec![],
                public_lookup: false,
            })
            .unwrap();
        Node::spawn_with_gc_interval(journal, Duration::from_secs(3600))
            .await
            .unwrap()
    }

    fn reference_missing_blob(node: &Node) {
        node.journal
            .store
            .append_local(
                node.journal.device_id(),
                crate::event::EventKind::Capture,
                Payload::Photo {
                    hash: Hash::from_bytes([0xAB; 32]).to_hex(),
                    size: 1024,
                    crypto: None,
                },
                false,
            )
            .unwrap();
    }

    async fn bind_reset_provider(handler: ResetBlobs) -> (Endpoint, Router, EndpointAddr) {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let router = Router::builder(endpoint.clone())
            .accept(iroh_blobs::ALPN, handler)
            .spawn();
        let addr = wait_addr(&endpoint).await;
        (endpoint, router, addr)
    }

    /// A burst of syncs must not open one blob download per sync.
    ///
    /// `fetch_missing_blobs` runs after every handshake, and the initiator
    /// closes the sync stream before the responder's download finishes, so a
    /// peer that reconnects in a loop stacks a download per attempt. Each
    /// attempt is its own QUIC connection, and a reset does not free it
    /// immediately.
    #[tokio::test]
    async fn overlapping_reset_fetches_stay_single_flight() {
        let dir = tempfile::tempdir().unwrap();
        let node = quiet_node(&dir.path().join("j")).await;
        reference_missing_blob(&node);

        let dials = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_inflight = Arc::new(AtomicUsize::new(0));
        let (_provider, _router, addr) = bind_reset_provider(ResetBlobs {
            dials: dials.clone(),
            streams: Arc::new(AtomicUsize::new(0)),
            inflight: inflight.clone(),
            max_inflight: max_inflight.clone(),
            hold: Duration::from_millis(400),
        })
        .await;

        let fetch = || {
            fetch_missing_blobs(
                &node.endpoint,
                &node.blobs,
                &node.journal,
                &addr,
                &node.fetch_gate,
            )
        };
        let (r1, r2, r3, r4) = tokio::join!(fetch(), fetch(), fetch(), fetch());
        r1.unwrap();
        r2.unwrap();
        r3.unwrap();
        r4.unwrap();

        assert_eq!(
            max_inflight.load(Ordering::SeqCst),
            1,
            "blob fetches from overlapping syncs must share one in-flight download"
        );
        assert_eq!(
            dials.load(Ordering::SeqCst),
            1,
            "a hash the peer just reset must not be dialed again immediately"
        );

        node.shutdown().await;
    }

    /// The same missing hash, asked again on the next sync, waits out a
    /// backoff instead of dialing on every handshake.
    #[tokio::test]
    async fn reset_blob_is_not_refetched_until_backoff_clears() {
        let dir = tempfile::tempdir().unwrap();
        let node = quiet_node(&dir.path().join("j")).await;
        reference_missing_blob(&node);

        let dials = Arc::new(AtomicUsize::new(0));
        let (_provider, _router, addr) = bind_reset_provider(ResetBlobs {
            dials: dials.clone(),
            streams: Arc::new(AtomicUsize::new(0)),
            inflight: Arc::new(AtomicUsize::new(0)),
            max_inflight: Arc::new(AtomicUsize::new(0)),
            hold: Duration::ZERO,
        })
        .await;

        let fetch = || {
            fetch_missing_blobs(
                &node.endpoint,
                &node.blobs,
                &node.journal,
                &addr,
                &node.fetch_gate,
            )
        };
        fetch().await.unwrap();
        fetch().await.unwrap();
        assert_eq!(
            dials.load(Ordering::SeqCst),
            1,
            "second sync must not redial a hash that just failed"
        );

        // A later sync, once the pause is lifted, tries again.
        node.fetch_gate.clear();
        fetch().await.unwrap();
        assert_eq!(dials.load(Ordering::SeqCst), 2);

        node.shutdown().await;
    }

    /// The same hash referenced by two captures is one download, not two.
    #[tokio::test]
    async fn duplicate_blob_refs_are_fetched_once() {
        let dir = tempfile::tempdir().unwrap();
        let node = quiet_node(&dir.path().join("j")).await;
        reference_missing_blob(&node);
        reference_missing_blob(&node);

        let streams = Arc::new(AtomicUsize::new(0));
        let (_provider, _router, addr) = bind_reset_provider(ResetBlobs {
            dials: Arc::new(AtomicUsize::new(0)),
            streams: streams.clone(),
            inflight: Arc::new(AtomicUsize::new(0)),
            max_inflight: Arc::new(AtomicUsize::new(0)),
            hold: Duration::ZERO,
        })
        .await;

        fetch_missing_blobs(
            &node.endpoint,
            &node.blobs,
            &node.journal,
            &addr,
            &node.fetch_gate,
        )
        .await
        .unwrap();
        assert_eq!(
            streams.load(Ordering::SeqCst),
            1,
            "two captures of one hash must open one blob stream"
        );
        node.shutdown().await;
    }

    #[test]
    fn blob_retry_backs_off_exponentially_and_caps() {
        assert_eq!(FetchGate::retry_delay(1), Duration::from_secs(15));
        assert_eq!(FetchGate::retry_delay(2), Duration::from_secs(30));
        assert_eq!(FetchGate::retry_delay(3), Duration::from_secs(60));
        assert_eq!(FetchGate::retry_delay(6), Duration::from_secs(480));
        assert_eq!(FetchGate::retry_delay(7), Duration::from_secs(15 * 60));
        assert_eq!(FetchGate::retry_delay(100), Duration::from_secs(15 * 60));
    }
}
