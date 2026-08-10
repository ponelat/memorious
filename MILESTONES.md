# Milestones

Build strictly in order. Each milestone ends with something working end to end and a short
demo note in `LOG.md`. Acceptance criteria are the contract — don't start milestone N+1 until
milestone N's criteria pass.

## M1 — Core: log, storage, sync

The engine, proven peer-to-peer with no UI.

- `crates/core`: event model (capture / redact / token-set / annotation), envelope
  (`event_id`, `device_id`, `seq`, `recorded_at`, `kind`, payload).
- SQLite event log + FTS5 index; iroh-blobs store for media; BLAKE3 hash refs in events.
- Sync protocol over iroh 1.x connections: exchange per-device heads (version vector),
  stream missing events both ways, fetch referenced blobs.
- Pairing: journal secret + ticket generation/redemption (string form; QR comes later).
- CLI harness (`journal` binary or `cargo run -p core --example cli`): init, add text/photo/
  audio from a file, list, redact, ticket, sync-with <ticket/addr>, status (per-peer heads).

**Accept:** two fresh peers on one machine (different data dirs) each capture entries
offline-of-each-other, sync, and both `list` outputs are identical, media included. Killing
one mid-sync and re-syncing converges. Automated two-peer convergence test in CI-runnable
`cargo test`.

## M2 — Server peer + web client

- `apps/server`: axum wrapping core. Endpoints: capture (text/photo/audio multipart), feed
  (paginated, reverse-chron), media fetch, trash/redact, sync status, token check. Serves the
  built `apps/web` bundle.
- Bearer-passcode auth end to end: passcode set via CLI/core → token-set event → server
  validates hash; browser stores token.
- `apps/web`: React capture + stream UI — text box, photo picker, mic recording
  (MediaRecorder → m4a/AAC), reverse-chron stream with derived day headers, polaroid fan for
  photo runs, audio play rows, trash view, FTS search box, sync status screen.

**Accept:** from a phone browser and a desktop browser: enter passcode once, capture all
three types, see them in the stream; server peer syncs with an M1 CLI peer and both converge.

## M3 — Desktop (Tauri 2)

- `apps/desktop`: Tauri 2 shell embedding core directly (its own peer, own local data —
  not a client of the server). Shared `apps/web` UI over a thin adapter (Tauri commands vs
  HTTP — same interface the browser build uses).
- Capture: text, paste/drop images, mic. Pairing UI (show/scan-paste ticket).

**Accept:** desktop app captures offline, then syncs with the server peer and an M1 CLI peer;
all three converge. UI is the same React code as M2 with only the adapter differing.

## M4 — iPhone (SwiftUI + UniFFI)

- UniFFI bindings for core; XCFramework build script checked in (`apps/ios/build.sh` spirit).
- SwiftUI app: capture text, camera photo (→JPEG), mic audio (→m4a); stream view; pairing
  via QR (show + scan); foreground-only sync (on open/foreground); sync status screen with
  undelivered badge.

**Accept:** iPhone captures offline in airplane mode; on reopen with network it syncs with
the server peer; entries appear in the web client. Photo and audio round-trip correctly.

## M5 — Enrichment + search surfacing

- Annotation events wired through core; `will_enrich` flag on capture.
- Scheduling rules exactly as `UNDERSTANDING.md`: sync-first, intent flag, ~15 min grace from
  local receipt, latest-annotation-wins (tie-break event id).
- Server sweeper: whisper transcription (audio), OCR (images), appending annotations.
- (Stretch) iPhone on-device enrichment via Apple Speech/Vision, setting the intent flag.
- Search UIs query FTS across text + annotations; annotations shown with their entries.

**Accept:** an audio note captured on the phone becomes searchable text in the web client
with no user action; two peers enriching the same entry converge on one winner.

## M6 — Import + export

- Import tool: v1 `GET /api/export` JSON → capture events with preserved `recorded_at`
  (v1 chunks flatten to plain entries; conversations are discarded; photos re-fetched and
  re-encoded to JPEG; idempotent re-runs).
- Derived markdown export: one-way `year/month/day.md` tree + media files, regenerable from
  the log on any peer, on demand.

**Accept:** owner's real v1 export imports cleanly; day files read sensibly; re-running
either tool changes nothing.
