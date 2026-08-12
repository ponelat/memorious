# Infinite Journal v2 — Shared Understanding

> **Naming (2026-08-11):** the product is now called **Memorious** (memorious.app).
> This founding document predates the name and is kept as written.

Agreed 2026-08-10 (grilling session). This is the founding document: what we're building and every
decision made, with the reasoning that mattered. Supersedes v1 idioms — read v1
(`~/projects/infinite-journal`) for history, not for design.

## What it is

A brutally minimalist, append-only capture device for **text, audio, and photos**. Local-first:
every app owns its full data and works offline. Peers sync directly with each other over
**Iroh** — no central server required. There is a server, but it is *just another peer* that
happens to be always on.

## Components (one core, three shells + web)

- **`crates/core`** — shared Rust crate holding the whole engine: event log, storage, sync
  protocol, enrichment scheduling. All the hard parts live here, once.
- **iPhone app** — native SwiftUI, calling the core via UniFFI-generated Swift bindings.
  Chosen over Tauri-iOS because capture ergonomics (camera, mic, share sheet) matter most on
  the phone.
- **Desktop app** — Tauri 2 wrapping the core, using the shared web UI.
- **Server peer** — the core running headless behind a small **axum** HTTP server. Always on:
  acts as backup, rendezvous point, browser gateway, and enrichment sweeper. No PocketBase.
- **Web client** — browsers can't speak Iroh, so the browser is a thin client of the server
  peer over plain HTTP/JSON.
- **Shared UI** — one React + Vite codebase used by both the Tauri shell and the browser
  client; a thin adapter layer switches between Tauri commands and HTTP calls.

Monorepo layout: Rust workspace (`crates/core`, `apps/server`, `apps/desktop`) plus `apps/web`
(React) and `apps/ios` (Xcode project + generated bindings).

## Data model

A **flat stream of events**. No conversations, no chunks, no stored grouping, no captions,
no tags, no digest/handled/inbox states. Any grouping (day headers, photo runs) is derived at
render time.

Event envelope: `event_id`, `device_id`, `seq` (per-device), `recorded_at`, `kind`, payload.

Event kinds:
- **capture** — text (inline), audio, or photo. Media payloads are BLAKE3 hash references into
  the blob store. Optional `will_enrich` flag (see Enrichment).
- **redact** — references an earlier entry; trash semantics: hidden everywhere, recoverable,
  media blob eventually garbage-collected. History is never rewritten, only struck through.
- **token-set** — sets the browser passcode (stores a **hash**, never the live secret).
  Exactly one passcode is active: latest event wins; concurrent offline sets resolved by log
  ordering (timestamp, device-id tiebreak). No revoke — only replace.
- **annotation** — transcription or OCR text referencing a media entry (see Enrichment).
  An annotation may instead target a **device id** (`dev-…` prefix keeps the namespaces
  apart): that is a device's editable friendly name, latest wins, synced like everything
  else — device naming without a fifth event kind (2026-08-12).

## Sync (DIY, no iroh-docs)

We deliberately skip iroh-docs (community-maintained, user doesn't want the dependency) and
build a tiny custom protocol on iroh connections:

1. Peers exchange per-device heads (version vectors: "latest seq I have per device").
2. Each sends the other its missing events.
3. Media blobs fetched via **iroh-blobs** (content-addressed).

Merging is a union of append-only logs — no conflicts exist by construction.

**Pairing & trust:** a journal is born on one device. Adding a device = showing a QR/ticket on
an existing device; the ticket carries the journal's shared secret + peer addresses. Possession
of the secret *is* identity. No accounts, no users table, anywhere.

**Relays:** config carries a relay list — n0's public relays **plus** a self-hosted iroh-relay
on our server (small Rust binary, needs domain + TLS) as fallback. Each device stores its own
network config in journal meta (2026-08-12): relay mode (n0 default / custom list / disabled)
and whether the peer publishes to the public address lookup (DNS/pkarr). Local, never syncs,
applied when the peer's endpoint spawns — i.e. on restart.

**iOS reality:** foreground sync only (on open / while active). No background-sync heroics in v1.

**Sync status UX:** every app has a status screen listing each known peer, last successful
sync, and whether it's missing anything we hold — plus a badge when this device has undelivered
local entries. Honest caveat: a peer's status is only as fresh as our last contact with it.
Peers also carry (2026-08-12): the device id ↔ endpoint id mapping and friendly name, how the
pair first met (who dialed whom), and — while the endpoint still holds a live path — the
transport in use (direct LAN / direct internet / public relay). The status screen additionally
shows journal stats (entry count, first→latest entry dates, disk usage of database and media
store) and the network config knobs above.

## Browser auth

Single bearer passcode, generated on any trusted peer → appended as a token-set event → syncs
to the server peer, which validates browser requests against the hash. Browser stores the
token in localStorage/cookie. No sessions, no email, no users.

## Media

Exactly one stored format per type, chosen for native-iOS-capture + universal playback:
- Audio: **AAC in m4a**.
- Photos: **JPEG**, normalized on capture. No originals kept.

## Storage (per peer)

- **SQLite** — the event log (canonical), indexes, and **FTS5** full-text search over entry
  text + annotations. FTS is derived, local, rebuildable, never syncs. Enabled from day one.
- **iroh-blobs fs store** — media blobs by hash.
- **Derived markdown export** — a one-way year/month/day file tree any peer can mirror to
  disk. Considered as *the* storage and rejected: day files are mutable aggregates that fight
  log-union sync. Canonical stays the log; the tree is an export.

QMD (tobi/qmd hybrid search) was evaluated and **rejected** — plain FTS is enough.

## Enrichment (transcription + OCR)

Enrichment results are just **annotation events** — they sync like everything else. The
coordination dance (agreed in detail):

1. **Capture always syncs immediately.** Enrichment never delays or holds data.
2. The capturing device may set a `will_enrich` flag on the capture event ("I intend to
   enrich this"). Peers that see the flag hold off. A peer that knows it can't/won't enrich
   (low battery, no model) simply omits the flag.
3. **Grace period measured from local receipt time** (~15 min), *not* capture time —
   otherwise an entry captured offline for a day arrives pre-expired and everyone piles on.
   After the grace period, any capable peer may enrich (in practice, the server sweeps).
4. Races produce duplicate annotations; harmless. **Latest annotation wins** (tie-break:
   event id). Latest-wins chosen deliberately: it makes upgrades free — re-run a better model
   next year, or append a manual correction, and it wins just by being newer.

Server enrichment stack: whisper for audio, an OCR pass for images.

## Reading UX

Reverse-chronological infinite stream. Derived day/time headers. **Polaroid fan-out kept from
v1** for runs of consecutive photos (compresses multiple images on the timeline). Audio as a
simple play row with duration. Full-screen photo on tap. Trash view for redacted entries.
Search box everywhere (FTS). Nothing else.

## Import

One-off tool that replays a v1 export (v1's `/api/export`) as events into a fresh v2 journal,
timestamps preserved. Mostly for testing with real data.

## Milestones

1. **Core** — event log, SQLite + blob storage, two peers syncing over Iroh; proven with a
   CLI harness.
2. **Server + web** — axum peer serving the React client; capture all three types from the
   browser; bearer-token auth.
3. **Desktop** — Tauri 2 wrapping the shared UI.
4. **iPhone** — SwiftUI + UniFFI bindings.
5. **Enrichment + search** — annotations, whisper/OCR sweeper on server, FTS UI.
6. **Import + export** — v1 import tool; derived markdown tree.

Each milestone leaves something working end to end.

## Encryption at rest (decided 2026-08-11)

**Threat model:** an attacker with read access to the disk (stolen laptop, shared machine,
server box image) learns nothing useful from the SQLite database, the blob store, or any
sidecar file. In-memory attacks while an app runs are out of scope.

### Key hierarchy

One **master password per journal**, shared by all paired devices (they must all derive the
same keys to read each other's media). It is entered once per device and cached in the OS
keychain where one exists (iOS/macOS); the headless server takes it from the environment.
The password does **not** travel in the pairing ticket — a ticket grants sync/replication,
the password grants reading. A photographed QR no longer exposes journal contents.

```
master password ──Argon2id(salt, params from keys.json)──▶ master key MK   (never on disk)
    ├── blake3 derive_key("memorious db key v1")   ──▶ SQLCipher database key
    └── blake3 derive_key("memorious wrap key v1") ──▶ key-wrapping key (KWK)
per blob: fresh random 32-byte content key (CK), wrapped by KWK into the capture event
```

- The Argon2id **salt is derived from the journal secret** (blake3 `derive_key`), so every
  peer computes identical keys with no coordination; it is copied into a plaintext
  `keys.json` in the journal root (with the KDF parameters) because the secret itself lives
  inside the encrypted database and must not be needed before unlock. The salt is not
  sensitive.
- **The manifest is the event log.** The proposal's external `manifest.enc` is redundant
  here: capture events already sync to every peer and now sit inside an encrypted database,
  so the wrapped CK + nonce base ride in the media payload (`Payload::Photo/Audio`). One
  sync mechanism, no second source of truth.
- Rotating the password = re-wrap CKs + re-key the database (future work; blobs never need
  re-encryption).

### SQLite: SQLCipher (community edition)

`rusqlite` with `bundled-sqlcipher-vendored-openssl`; the database key is applied with
`PRAGMA key = "x'…'"` (raw-key form — we already did the password stretching; SQLCipher's
own KDF would just re-stretch). FTS5 works unchanged — the whole file, FTS index included,
is encrypted. Wrong password surfaces as SQLCipher's "file is not a database" and is
reported as such.

### Blobs: encrypt at ingest, ciphertext is the identity

All encryption happens **before** `add_bytes`: the iroh-blobs store only ever contains
ciphertext, and the BLAKE3 hash / outboard / sync identity are of the ciphertext. No
store-level shim, nothing to leak. Peers exchange ciphertext blobs as before; the wrapped
CK arrives with the capture event.

Format (all media): plaintext split into 64 KiB chunks, each sealed with
XChaCha20-Poly1305; nonce = 19-byte random base ‖ 4-byte big-endian chunk counter ‖ 1-byte
final flag (the STREAM construction — reordering, truncation, and cross-blob splicing all
fail authentication). `size` in the payload stays the plaintext length.

### Faces

- **CLI**: `--password`, `MEMORIOUS_PASSWORD`, or interactive prompt.
- **Server**: `MEMORIOUS_PASSWORD` env var, required. Browser flow unchanged — the server
  holds the keys and serves decrypted media over the existing passcode-authed API.
- **Desktop**: unlock screen on first launch; password cached in the OS keychain.
- **iPhone**: unlock/setup screens; password cached in the iOS Keychain.
- **Enrichment** shells out via temp files: 0700 tempdirs, plaintext zero-overwritten
  best-effort before deletion.

### Migration & compatibility

Encrypted journals are the only kind the engine opens (single code path; a journal without
`keys.json` gets a pointed error). `memorious migrate-encrypt` rebuilds a plaintext journal
in place — same journal secret, device id, endpoint key, event ids/seqs/timestamps;
media re-encrypted under fresh CKs (new blob hashes, payloads rewritten) — leaving the old
directory beside it as a backup. Peers re-pair fresh, exactly like the protocol rename.
Sync ALPN bumps to `memorious/sync/1`; media payloads carry the crypto fields, so old and
new builds don't interoperate (deliberate, all peers are owner-controlled).
