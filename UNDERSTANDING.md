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
on our server (small Rust binary, needs domain + TLS) as fallback.

**iOS reality:** foreground sync only (on open / while active). No background-sync heroics in v1.

**Sync status UX:** every app has a status screen listing each known peer, last successful
sync, and whether it's missing anything we hold — plus a badge when this device has undelivered
local entries. Honest caveat: a peer's status is only as fresh as our last contact with it.

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
