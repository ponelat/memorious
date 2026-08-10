//! A running peer: journal + blob store + iroh endpoint speaking the sync protocol.
//!
//! Protocol (ALPN `infinite-journal/sync/0`), one bi stream per sync:
//!   initiator → Hello { auth, heads, addr }     auth = keyed blake3 of the journal secret
//!   responder → HelloAck { heads }              (or closes the connection on bad auth)
//!   responder → Event* EndEvents                events the initiator is missing
//!   initiator → Event* EndEvents                events the responder is missing
//!   responder → Done
//! Afterwards each side pulls any referenced-but-absent blobs over the iroh-blobs ALPN
//! (the responder dials back using the addr carried in Hello). Fetching *all* missing
//! referenced blobs each sync also heals earlier interrupted transfers.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_blobs::api::Store as BlobStore;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobsProtocol, Hash};
use serde::{Deserialize, Serialize};

use crate::event::{Event, MediaKind, Payload};
use crate::journal::{Journal, SECRET_LEN};
use crate::store::Heads;

pub const SYNC_ALPN: &[u8] = b"infinite-journal/sync/0";
const AUTH_CONTEXT: &[u8; 32] = b"infinite-journal auth v0 ctx key";
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
    },
    HelloAck {
        heads: Heads,
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

/// Ticket: journal secret + address of one existing peer. String form `journal<base32>`.
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

const TICKET_PREFIX: &str = "journal";

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
        let journal = Arc::new(journal);
        let fs_store = FsStore::load(journal.blobs_dir()).await?;
        let blobs: BlobStore = fs_store.clone().into();

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

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .bind()
            .await
            .context("bind iroh endpoint")?;

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

    /// Redeem a ticket: create the local journal from its secret, then pull everything.
    pub async fn join_from_ticket(root: &Path, ticket: &str) -> Result<(Self, SyncReport)> {
        let ticket = JournalTicket::decode(ticket)?;
        let peer_addr = ticket.addr()?;
        let journal = Journal::init_with_secret(root, ticket.secret)?;
        let node = Self::spawn(journal).await?;
        let report = node.sync_with(&peer_addr).await?;
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
        let size = bytes.len() as u64;
        let tag = self.blobs.add_bytes(bytes).await?;
        self.journal.store.append_local(
            self.journal.device_id(),
            crate::event::EventKind::Capture,
            Payload::media(kind, tag.hash.to_hex().to_string(), size),
            will_enrich,
        )
    }

    /// Media capture with an explicit `recorded_at` (import tool).
    pub async fn capture_blob_at(
        &self,
        kind: MediaKind,
        bytes: Vec<u8>,
        recorded_at: i64,
    ) -> Result<Event> {
        let size = bytes.len() as u64;
        let tag = self.blobs.add_bytes(bytes).await?;
        self.journal.store.append_local_at(
            self.journal.device_id(),
            crate::event::EventKind::Capture,
            Payload::media(kind, tag.hash.to_hex().to_string(), size),
            false,
            recorded_at,
        )
    }

    /// Whole blob, by hex hash.
    pub async fn blob_bytes(&self, hash_hex: &str) -> Result<Vec<u8>> {
        let hash: Hash = hash_hex.parse().context("bad blob hash")?;
        Ok(self.blobs.get_bytes(hash).await?.to_vec())
    }

    pub async fn has_blob(&self, hash_hex: &str) -> Result<bool> {
        let hash: Hash = hash_hex.parse().context("bad blob hash")?;
        Ok(self.blobs.has(hash).await?)
    }

    /// One full sync round-trip with the peer at `addr`.
    pub async fn sync_with(&self, addr: &EndpointAddr) -> Result<SyncReport> {
        let conn = self
            .endpoint
            .connect(addr.clone(), SYNC_ALPN)
            .await
            .context("connect to peer")?;
        let (mut send, mut recv) = conn.open_bi().await?;

        let my_addr = self.dialable_addr().await.unwrap_or_else(|_| self.endpoint.addr());
        write_msg(
            &mut send,
            &Msg::Hello {
                auth: auth_token(self.journal.secret()),
                heads: self.journal.store.heads()?,
                addr: AddrWire::from_addr(&my_addr),
            },
        )
        .await?;

        let peer_heads = match read_msg(&mut recv).await? {
            Some(Msg::HelloAck { heads }) => heads,
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

        report.blobs_fetched = fetch_missing_blobs(&self.endpoint, &self.blobs, &self.journal, addr)
            .await
            .context("fetch blobs")?;

        conn.close(VarInt::from(0u32), b"done");
        Ok(report)
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
    let mut missing = Vec::new();
    for hex in journal.store.referenced_blob_hashes()? {
        let hash: Hash = hex.parse().context("bad hash in log")?;
        if !blobs.has(hash).await? {
            missing.push(hash);
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    let conn = endpoint
        .connect(provider.clone(), iroh_blobs::ALPN)
        .await
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

        let (heads, initiator_addr) = match read_msg(&mut recv).await.map_err(acc)? {
            Some(Msg::Hello { auth, heads, addr }) => {
                if auth != auth_token(self.journal.secret()) {
                    connection.close(VarInt::from(CLOSE_BAD_AUTH), b"bad auth");
                    return Err(AcceptError::from_err(std::io::Error::other(
                        "peer failed journal auth",
                    )));
                }
                (heads, addr.to_addr().map_err(acc)?)
            }
            _ => {
                return Err(AcceptError::from_err(std::io::Error::other(
                    "protocol violation: expected Hello",
                )))
            }
        };

        let my_heads = self.journal.store.heads().map_err(acc)?;
        write_msg(&mut send, &Msg::HelloAck { heads: my_heads })
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
        assert!(s.starts_with("journal"));
        let back = JournalTicket::decode(&s).unwrap();
        assert_eq!(back.secret, ticket.secret);
        let back_addr = back.addr().unwrap();
        assert_eq!(back_addr.id, addr.id);
        assert_eq!(back_addr.ip_addrs().count(), 1);
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
