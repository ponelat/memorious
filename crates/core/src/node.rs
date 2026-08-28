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

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_blobs::api::Store as BlobStore;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobsProtocol, Hash};
use serde::{Deserialize, Serialize};

use crate::event::{AudioKind, Event, MediaKind, Payload};
use crate::journal::{Journal, SECRET_LEN};
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
    fn from_addr(addr: &EndpointAddr) -> Self {
        Self {
            id_hex: addr.id.to_string(),
            relays: addr.relay_urls().map(|u| u.to_string()).collect(),
            ips: addr.ip_addrs().map(|a| a.to_string()).collect(),
        }
    }

    fn to_addr(&self) -> Result<EndpointAddr> {
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
    },
    HelloAck {
        heads: Heads,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
    },
    Event(Event),
    EndEvents,
    Done,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SyncReport {
    pub sent: usize,
    pub received: usize,
    pub blobs_fetched: usize,
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

pub struct Node {
    journal: Arc<Journal>,
    blobs: BlobStore,
    endpoint: Endpoint,
    router: Router,
    _fs_store: FsStore,
}

impl Node {
    /// Bind an endpoint, register sync + blobs protocols, and start accepting.
    pub async fn spawn(journal: Journal) -> Result<Self> {
        Self::spawn_with_gc_interval(journal, DEFAULT_GC_INTERVAL).await
    }

    /// `spawn` with an explicit blob-GC cadence (tests use a fast one).
    pub async fn spawn_with_gc_interval(journal: Journal, gc_interval: Duration) -> Result<Self> {
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

        let proto = SyncProto {
            journal: journal.clone(),
            blobs: blobs.clone(),
            endpoint: endpoint.clone(),
        };
        let blobs_proto = BlobsProtocol::new(&blobs, None);
        let router = Router::builder(endpoint.clone())
            .accept(SYNC_ALPN, proto)
            .accept(iroh_blobs::ALPN, blobs_proto)
            .spawn();

        Ok(Self {
            journal,
            blobs,
            endpoint,
            router,
            _fs_store: fs_store,
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
        let node = Self::spawn(journal).await?;
        let report = node.sync_events_with(&peer_addr).await?;
        for ev in node.journal.store.all_events()? {
            if let Some(crypto) = ev.payload.blob_crypto() {
                node.journal
                    .unwrap_blob_keys(crypto)
                    .context("master password doesn't match this journal")?;
                break;
            }
        }
        Ok((node, report))
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
        report.blobs_fetched =
            fetch_missing_blobs(&node.endpoint, &node.blobs, &node.journal, &peer_addr)
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
    pub async fn blob_bytes(&self, hash_hex: &str) -> Result<Vec<u8>> {
        let hash: Hash = hash_hex.parse().context("bad blob hash")?;
        let payload = self
            .journal
            .store
            .capture_payload_for_hash(hash_hex)?
            .context("no capture references this blob")?;
        let envelope = payload
            .blob_crypto()
            .context("media predates encryption — run `memorious migrate-encrypt`")?;
        let (ck, nonce_base) = self.journal.unwrap_blob_keys(envelope)?;
        let ciphertext = self.blobs.get_bytes(hash).await?;
        tokio::task::spawn_blocking(move || crate::crypto::open(&ciphertext, &ck, &nonce_base))
            .await?
    }

    pub async fn has_blob(&self, hash_hex: &str) -> Result<bool> {
        let hash: Hash = hash_hex.parse().context("bad blob hash")?;
        Ok(self.blobs.has(hash).await?)
    }

    /// One full sync round-trip with the peer at `addr`: events, then media.
    pub async fn sync_with(&self, addr: &EndpointAddr) -> Result<SyncReport> {
        let mut report = self.sync_events_with(addr).await?;
        report.blobs_fetched = fetch_missing_blobs(&self.endpoint, &self.blobs, &self.journal, addr)
            .await
            .context("fetch blobs")?;
        Ok(report)
    }

    /// Event-log-only round-trip: timelines converge, media stays deferred.
    pub async fn sync_events_with(&self, addr: &EndpointAddr) -> Result<SyncReport> {
        let conn = tokio::time::timeout(SYNC_CONNECT_TIMEOUT, self.endpoint.connect(addr.clone(), SYNC_ALPN))
            .await
            .map_err(|_| {
                anyhow::anyhow!("peer unreachable ({}s)", SYNC_CONNECT_TIMEOUT.as_secs())
            })?
            .context("connect to peer")?;
        let (mut send, mut recv) = conn.open_bi().await?;

        let my_addr = self.dialable_addr().await.unwrap_or_else(|_| self.endpoint.addr());
        write_msg(
            &mut send,
            &Msg::Hello {
                auth: auth_token(self.journal.secret()),
                heads: self.journal.store.heads()?,
                addr: AddrWire::from_addr(&my_addr),
                device: Some(self.journal.device_id().to_string()),
            },
        )
        .await?;

        let (peer_heads, peer_device) = match read_msg(&mut recv).await? {
            Some(Msg::HelloAck { heads, device }) => (heads, device),
            Some(other) => bail!("protocol violation: expected HelloAck, got {other:?}"),
            None => bail!("peer closed during handshake (bad journal secret?)"),
        };

        // Receive what we're missing.
        let mut report = SyncReport::default();
        loop {
            match read_msg(&mut recv).await? {
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
        for ev in self.journal.store.events_missing_from(&peer_heads)? {
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
        self.journal.record_sync_contact(
            &addr.id.to_string(),
            unix_now_ms(),
            peer_device.as_deref(),
            "ticket",
        )?;
        self.remember_peer_addr(addr);
        Ok(report)
    }

    /// Known sync peers with their device mapping, last contact, and (when
    /// the endpoint still holds a live path) the transport in use.
    pub async fn peers(&self) -> Result<Vec<PeerInfo>> {
        let store = &self.journal.store;
        let mut out = Vec::new();
        for (key, value) in store.meta_scan("peer_last_ok:")? {
            let endpoint_id = key.trim_start_matches("peer_last_ok:").to_string();
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
            out.push(PeerInfo {
                endpoint_id,
                device_id,
                last_ok_ms,
                discovery,
                conn,
            });
        }
        out.sort_by_key(|p| std::cmp::Reverse(p.last_ok_ms));
        Ok(out)
    }

    /// The status screen's JSON, one shape for every face (HTTP, Tauri, FFI).
    pub async fn status_json(&self) -> Result<serde_json::Value> {
        let journal = self.journal();
        let timeline = journal.timeline()?;
        let mut v = serde_json::json!({
            "device_id": journal.device_id(),
            "entries": timeline.entries,
            "trash": journal.trash()?.len(),
            "heads": journal.store.heads()?,
            "timeline": timeline,
            "storage": journal.storage_usage()?,
            "health": journal.sync_health(unix_now_ms())?,
            "names": journal.device_names()?,
            "peers": self.peers().await?,
            "net": journal.net_config(),
        });
        if let Ok(t) = self.ticket() {
            v["ticket"] = t.into();
        }
        Ok(v)
    }

    pub async fn shutdown(self) {
        let _ = self.router.shutdown().await;
        let _ = self.blobs.shutdown().await;
        self.endpoint.close().await;
    }
}

/// Pull every blob referenced by the log that we don't hold, from `provider`.
async fn fetch_missing_blobs(
    endpoint: &Endpoint,
    blobs: &BlobStore,
    journal: &Journal,
    provider: &EndpointAddr,
) -> Result<usize> {
    // Retention: don't pull back what this device's policy says it may drop
    // (retention.rs) — the GC would only remove it again.
    let evictable: HashSet<String> = journal
        .evictable_blob_hashes(&journal.retention_policy()?, unix_now_ms())?
        .into_iter()
        .collect();
    let mut missing = Vec::new();
    for hex in journal.store.referenced_blob_hashes()? {
        if evictable.contains(&hex) {
            continue;
        }
        let hash: Hash = hex.parse().context("bad hash in log")?;
        if !blobs.has(hash).await? {
            missing.push(hash);
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    let conn = tokio::time::timeout(SYNC_CONNECT_TIMEOUT, endpoint.connect(provider.clone(), iroh_blobs::ALPN))
        .await
        .map_err(|_| anyhow::anyhow!("peer unreachable ({}s)", SYNC_CONNECT_TIMEOUT.as_secs()))?
        .context("connect for blobs")?;
    let mut fetched = 0;
    for hash in missing {
        match blobs.remote().fetch(conn.clone(), hash).await {
            Ok(_) => fetched += 1,
            // The provider may legitimately not hold this blob (it came from a third
            // peer). Sync must not fail over it; a later sync will heal it.
            Err(err) => tracing::warn!("blob {} not fetched: {err:#}", hash.fmt_short()),
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
        ping
    }

    /// Probe every known peer concurrently. Order matches `peers()`
    /// (most recently heard from first).
    pub async fn ping_peers(&self, timeout: Duration) -> Result<Vec<PeerPing>> {
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
}

impl std::fmt::Debug for SyncProto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncProto").finish_non_exhaustive()
    }
}

impl ProtocolHandler for SyncProto {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;

        let (heads, initiator_addr, initiator_device) =
            match read_msg(&mut recv).await.map_err(acc)? {
                Some(Msg::Hello { auth, heads, addr, device }) => {
                    if auth != auth_token(self.journal.secret()) {
                        connection.close(VarInt::from(CLOSE_BAD_AUTH), b"bad auth");
                        return Err(AcceptError::from_err(std::io::Error::other(
                            "peer failed journal auth",
                        )));
                    }
                    (heads, addr.to_addr().map_err(acc)?, device)
                }
                _ => {
                    return Err(AcceptError::from_err(std::io::Error::other(
                        "protocol violation: expected Hello",
                    )))
                }
            };

        let my_heads = self.journal.store.heads().map_err(acc)?;
        write_msg(
            &mut send,
            &Msg::HelloAck {
                heads: my_heads,
                device: Some(self.journal.device_id().to_string()),
            },
        )
        .await
        .map_err(acc)?;

        // Send the initiator what it's missing.
        let to_send = self
            .journal
            .store
            .events_missing_from(&heads)
            .map_err(acc)?;
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
                Some(Msg::Event(ev)) => {
                    self.journal
                        .store
                        .insert_remote(&ev)
                        .map_err(acc)?;
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
        if let Ok(json) = serde_json::to_vec(&AddrWire::from_addr(&initiator_addr)) {
            let _ = self
                .journal
                .store
                .meta_set(&format!("peer_addr:{}", connection.remote_id()), &json);
        }

        // Pull blobs we now reference but don't hold, dialing back the initiator.
        if let Err(err) =
            fetch_missing_blobs(&self.endpoint, &self.blobs, &self.journal, &initiator_addr).await
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
        assert!(matches!(msg, Msg::Hello { device: None, .. }));
        let old_ack = serde_json::json!({ "HelloAck": { "heads": crate::store::Heads::new() } });
        let msg: Msg = serde_json::from_value(old_ack).unwrap();
        assert!(matches!(msg, Msg::HelloAck { device: None, .. }));
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
}
