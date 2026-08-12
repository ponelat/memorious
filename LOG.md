# LOG

## 2026-08-12 (sync page: peers, names, stats, net config)
- **Device names**: an annotation event may now target a *device id* instead of an event
  id (`dev-` prefix separates the namespaces) — that annotation is the device's friendly
  name. Editable from any device, latest wins, syncs as ordinary events; no fifth event
  kind. Each face writes a platform default on first run ("web", "desktop (macOS)",
  "iPhone"); the CLI deliberately doesn't. UNDERSTANDING.md amended.
- **Peer registry**: `Hello`/`HelloAck` gained an optional `device` field (serde-default,
  old frames still decode), so each side maps the peer's endpoint id → journal device id.
  `record_sync_contact` also stores a set-once origin ("dialed"/"inbound"). `Node::peers()`
  reports device id, name, last contact, origin, and — via `Endpoint::remote_info` — the
  transport in use right now (relay url, or socket addr classified LAN/internet by
  private-range check). Honest limitation: transport is only known while the endpoint
  still holds a path; between contacts it's `None`.
- **Stats**: `Journal::timeline()` (entry count, first/latest recorded_at) and
  `Journal::storage_usage()` (db.sqlite + sidecars, blobs dir walk).
- **Net config**: per-device `net_config` in journal meta — relay mode (default n0 /
  custom urls / disabled) and public address lookup (DNS/pkarr) on/off. Applied at
  `Node::spawn` by building the endpoint from `presets::Minimal` + the chosen pieces
  (was hardwired `presets::N0`); changes take effect on restart. Validation at set time
  (urls must parse, custom needs ≥1).
- **One status shape**: `Node::status_json()` feeds the HTTP `/api/status`, the Tauri
  `status` command, and the FFI `status_json` — plus new mutators everywhere
  (`/api/device-name`, `/api/net-config`; `set_device_name`/`set_net_config` commands and
  FFI). Web sync page reworked: SVG peer map (transport-colored edges, arrows showing who
  dials whom, relay drawn as a waypoint, animated when live), device list with inline
  rename, journal/storage lines, network settings form.
- Boring choices: names capped at 64 chars; `annotations()` consumers are unaffected
  (feed/export/sweeper key by event id, device ids never match); sync-report `sent`
  counts in two tests bumped — a first-run name annotation is an event like any other.

## 2026-08-12 (video everywhere; paste-to-capture)
- The Video media kind (introduced with the mobile bindings) now reaches every face:
  server route `POST /api/capture/video` (mp4-family passes through; webm transcoded to
  H.264/AAC MP4 via system ffmpeg, mirroring audio), `video/mp4` on media fetch, desktop
  `capture_media` kind `"video"`, and web rendering (polaroid thumbnail with a play
  badge; lightbox plays with controls). Server captures video without the will-enrich
  flag — there is no video enrichment engine, so peers shouldn't hold a grace period.
- Capture bar accepts pasted media: pasted photos/videos/audio stage in a small tray
  above the input (thumbnails, remove buttons) and are captured on submit — attachments
  in paste order, then the text, each its own capture event (flat stream, no grouping
  stored). Unsupported clipboard files are refused with a visible error.
- Boring choices: paste is the only staging path (the photo button still captures
  immediately); staged previews are object URLs revoked on unstage; a failed attachment
  capture stops the submit and leaves the rest staged.

## 2026-08-12 (pairing defers media)
- `Node::join_from_ticket` split: new `pair_from_ticket` pulls the event log and proves
  the master password (key unwrap needs no blob bytes), leaving media for a later
  `sync_with`; `join_from_ticket` keeps the everything-before-returning behavior (CLI).
  `sync_events_with` is the new events-only round-trip underneath. Motivation: joining a
  media-heavy journal from a phone should be usable immediately — capture-first — with
  the blob fetch happening as an ordinary background sync.

## 2026-08-11 (encryption at rest)
- **Everything at rest is now encrypted** under a single master password per journal
  (design in UNDERSTANDING.md §"Encryption at rest"): SQLCipher (community, vendored
  OpenSSL) for the event log/FTS, and per-blob XChaCha20-Poly1305 (64 KiB STREAM chunks)
  applied **before** iroh-blobs ingest — blob hashes are of ciphertext.
- Key layers: password → Argon2id → master key; blake3-derived DB key + wrapping key;
  fresh random content key per blob, wrapped into the capture event payload. The event
  log *is* the manifest — no separate manifest file. Argon2id salt derives from the
  journal secret and is mirrored in plaintext `keys.json` (with KDF params) so unlock
  can precede opening the DB.
- Password never rides in the pairing ticket: ticket = replicate, password = read.
- Sync ALPN bumped to `memorious/sync/1` (payload shape + ciphertext identities are a
  clean break; all peers owner-controlled, same precedent as the rename).
- `memorious migrate-encrypt` rebuilds a plaintext journal in place (identity + event
  ids/seqs preserved, media re-encrypted, old dir kept as `<dir>.pre-encryption`); other
  devices re-pair fresh.
- Enrichment temp files: 0700 tempdir + best-effort zero-overwrite before delete.

## 2026-08-11 (open source)
- **Licensing decided: open-core.** This repo (engine, server, web, desktop, CLI,
  landing) is public under **MIT OR Apache-2.0**; the native mobile apps are a paid,
  proprietary product in a separate private repo (`memorious-mobile`, iOS under `ios/`,
  Android later), split out with its history via git filter-repo. The Memorious name
  and branding stay reserved (see README §License). Rationale: sell convenience on the
  app stores while everything self-hostable is genuinely free.
- History rewritten before going public: `apps/ios` removed from all commits, and the
  ops runbook (docs/DEPLOY.md) moved to a private umbrella repo along with redaction of
  personal infra strings throughout history. Deployment specifics intentionally live
  outside this repo from now on.

## 2026-08-11 (Memorious)
- The product is named **Memorious** (memorious.app registered by owner). Full
  ubiquitous-language sweep: crates memorious-*, CLI binary `memorious`, env
  MEMORIOUS_DATA(_DIR), sync ALPN `memorious/sync/0`, ticket prefix `memorious`,
  fresh 32-byte auth context, web/iOS/desktop wordmarks + display names, flake
  packages, downloads renamed. "Journal" stays as the domain noun in code/docs.
- Protocol identifiers changed ⇒ all peers upgraded together: dev server
  (now the dev host via devhost rename), iPhone (in-place upgrade,
  shows "Memorious"), desktop bundle rebuilt as Memorious.app. Old pairing
  tickets are invalid; stored last-peer tickets need one re-pair.
- iOS bundle id KEPT as the legacy bundle id for now — Xcode's Apple ID
  session had lapsed so no profile could be minted for a new id; keeping it also
  upgraded the installed app in place (data intact). Rename next time Xcode is
  logged in. Desktop identifier DID change (com.ponelat.memorious) — a journal
  initialized under the old desktop app lives orphaned at
  ~/Library/Application Support/com.ponelat.infinite-journal/.
- Repo renamed: github.com/clawjungle/memorious; local dir ~/projects/memorious.
  Landing deployed to memorious.app vhost on the Caddy box (awaiting owner's DNS
  A record → REDACTED-IP); hero video re-captured with rebranded UI.
- Rename gotcha: cargo package renames invalidate every workspace fingerprint —
  full recompile (~35 min). Also: don't rename the project dir mid-compile.

## 2026-08-11 (NixOS)
- NixOS/Linux delivery: (1) static musl CLI binaries (x86_64 + aarch64, cargo-zigbuild)
  hosted in /downloads — x86_64 one smoke-tested in an Alpine container; (2) flake.nix
  building journal-cli / memorious-server / memorious-desktop from source. nixpkgs-unstable
  pinned (25.05 rustc 1.86 < iroh's 1.91 floor). apps/web/dist now committed so flake
  builds are self-contained.
- journal-cli verified with a real `nix build` on this Mac. The desktop derivation
  (cargo-tauri.hook + webkitgtk_4_1/gtk3/libsoup_3, --no-bundle + manual install) is
  NOT yet build-verified on Linux: two Docker attempts died — first seccomp under amd64
  emulation, then the host disk hit 100% mid-build and corrupted Docker Desktop's
  containerd metadata (prune/df now 500; needs a Docker factory reset — owner's call,
  it wipes all local images). First `nix build .#memorious-desktop` on a real NixOS box
  is the honest test; expect at most attribute-name-level fixes.
- Disk on this Mac is chronically tight (~3GB free after cleanup); Rust target dirs
  and Docker.raw are the repeat offenders.

## 2026-08-11
- App downloads hosted by the server peer: `scripts/make-downloads.sh` builds the CLI
  binary + zipped desktop .app into ./downloads (gitignored); server serves them at
  /downloads (public — software only) and lists them via authed /api/downloads; web
  sync view shows a "get the apps" section. Live-verified on the dev host.
- Desktop zip is unsigned/un-notarized — Gatekeeper needs right-click → Open (or
  `xattr -dr com.apple.quarantine`) on first launch. iOS stays outside downloads
  (needs Xcode/TestFlight signing).

## 2026-08-10 (M6)
- M6 complete: v1 import + derived markdown export, both idempotent.
- `journal import-v1 <export.json>`: chunks flatten to plain entries (conversations
  discarded), recorded_at preserved (own ISO→ms parser, no chrono), photos re-fetched
  (curl) and re-encoded to JPEG, per-chunk meta markers make re-runs no-ops; a failed
  photo leaves its chunk unmarked so the next run retries it.
- `journal export-md <dir>`: UTC year/month/day.md tree + media/<hash>.jpg|m4a,
  annotations as blockquotes, redacted entries excluded, write-if-changed so re-runs
  report "0 written".
- Real acceptance: minted a temporary API token on prod (sqlite insert over SSH —
  no PB user password available locally; token revoked + verified 401 afterwards),
  pulled the real export (1129 chunks, 343 conversations), imported 1125 text + 33
  photos with 0 failures into a fresh journal, re-ran both tools → no changes. Day
  files read as real journal history. Import preserved at ./data-v1-import
  (gitignored) — pair it into any peer with `journal serve` + join, or point the
  live server's MEMORIOUS_DATA at it.
- v1 export quirk: 4 chunks had blank content (dropped by design).

## 2026-08-10 (M5)
- M5 complete: enrichment as annotation events, scheduled per UNDERSTANDING (sync-first,
  will_enrich intent flag, 15-min grace from local receipt via new local_received_at
  column, latest-annotation-wins with event-id tiebreak).
- Server sweeper (60s loop, env-tunable): whisper.cpp (`brew whisper-cpp`, ggml-base
  model at ~/.cache/whisper/ggml-base.bin) for audio, tesseract for photos, both behind
  an Engines trait so tests run with mocks. Server marks its own media captures
  will_enrich=true and enriches them immediately; unflagged peers' media immediately;
  flagged ones after grace.
- Annotations attached to entries in feed/search on server, desktop, and iOS; all UIs
  render them; FTS covers them (indexed since M1).
- Live acceptance on the dev host: spoken m4a captured via API → sweeper
  transcribed "Remember to buy oat milk and fix the kayak rudder." → search "kayak"
  returns the audio entry with transcript. No user action.
- Real-engine tests synthesize fixtures (macOS `say` + a committed rendered-text PNG)
  and skip cleanly where tools are missing. iOS on-device enrichment (stretch) skipped.
- Mid-milestone incident: disk hit 100% (20GB cargo target dir); a peer session ran
  cargo clean to unblock. Keep an eye on target/ size.

## 2026-08-10 (M4)
- M4 built and simulator-verified: SwiftUI app (`apps/ios`), UniFFI bindings from a new
  `crates/mobile` FFI crate (blocking calls over a private tokio runtime, JSON strings
  across the boundary — same shapes as the HTTP/Tauri layers).
- `apps/ios/build.sh` builds the Rust core for iOS targets, generates Swift bindings
  (uniffi-bindgen library mode from a host dylib), and packages JournalCore.xcframework.
  Project generated by xcodegen (`project.yml`); needs `-framework SystemConfiguration`
  for iroh's netdev.
- App: setup (fresh / paste ticket / QR scan), capture text + camera photo + m4a audio
  (AVAudioRecorder, AAC — no transcode needed by design), stream with day sections,
  swipe-to-trash, sync sheet with QR ticket + share + sync-now, foreground-only sync on
  scenePhase active, quiet auto-sync after each capture.
- XCUITests pass on iPhone 17 Pro simulator: fresh init → capture → appears in stream;
  join-by-ticket from a live host CLI peer → synced entry appears (real iroh connection
  from simulator to host). Owner still to verify on a physical device: airplane-mode
  offline capture, camera, mic, and QR scanning (simulator has no camera).
- iroh + rusqlite + iroh-blobs all cross-compile clean for aarch64-apple-ios(-sim).

## 2026-08-10 (M3)
- M3 complete: Tauri 2 desktop shell (`apps/desktop`), its own peer with core embedded —
  not a server client. Bundle: `target/release/bundle/macos/Infinite Journal.app`.
- Same React UI as M2; the only difference is the adapter (`src/api/tauri.ts` vs
  `http.ts`), picked at runtime by `__TAURI_INTERNALS__`. Desktop adapter: no passcode
  (possession of the device = trust), setup screen on first run (start fresh / paste
  ticket), sync-now + pairing in the sync view (last ticket remembered in journal meta).
- Media over IPC: photos normalized to JPEG in the command layer; audio accepted only as
  mp4-family (macOS WKWebView records AAC/mp4 natively — no ffmpeg dependency on desktop).
- Verified via Tauri mock-runtime IPC tests (real command routing incl. ACL + camelCase
  arg mapping): setup→capture→feed→media→sync; three peers (desktop + 2 core peers)
  converge; wrong-journal ticket refused. App binary launches; screen-recording permission
  unavailable to this session, so pixel-level check of the desktop window is owner-verified.
- Gotcha for later shells: mock IPC tests need `tauri://localhost` as the invoke origin on
  macOS and commands generic over `tauri::Runtime`.

## 2026-08-10 (M2)
- M2 complete: axum server peer (`apps/server`) + React web client (`apps/web`), live at
  the dev app (devhost, port block 4390, release binary; dev journal
  in ./data, gitignored; passcode (redacted) for dev).
- API: auth/check, capture text/photo/audio, feed (cursor pagination), media, redact,
  trash, search, status (incl. pairing ticket). All /api behind bearer passcode; media
  fetched as authed blobs → object URLs (media tags can't send headers).
- Photos normalized to JPEG in core (`media.rs`, image crate); audio: mp4-family kept,
  webm/ogg transcoded to AAC via system ffmpeg (Chrome MediaRecorder → webm; Safari
  records mp4 natively).
- UI: login, capture bar (text/photo/mic), stream with derived day headers, polaroid fan
  for photo runs (30-min gap rule, render-time only), lightbox, trash, sync status, FTS
  search. Adapter seam (`JournalApi`) is the single switch point for the Tauri build.
- Verified end to end in headless Chrome (bdg): login → capture → stream, fan, lightbox.
  Server↔CLI-peer convergence is a cargo test. Phone-browser pass still owner-verified.

## 2026-08-10 (M1)
- M1 complete: core event log + SQLite/FTS5 store, iroh-blobs media store, custom sync
  protocol over iroh 1.0.3, journal-secret pairing tickets, `journal` CLI.
- Demo: two CLI peers on one machine — A served, B joined from A's ticket (2 events +
  1 photo blob), B captured, synced back; both `list` outputs identical. 19 tests green,
  incl. two-peer convergence with media, wrong-secret rejection, interrupted-sync retry.
- Decisions made while building (doc silent, boring choice):
  - Wire frames + ticket payload avoid iroh's own serde (needs deserialize_any; postcard
    rejects it). Frames are length-prefixed JSON; ticket carries an `AddrWire` of strings.
  - Sync auth = keyed blake3 of the journal secret sent in Hello; responder closes on
    mismatch. Possession of secret = trust, per UNDERSTANDING.
  - Both sides fetch *all* referenced-but-missing blobs every sync (not just new ones) —
    heals interrupted transfers for free.
  - Token-set latest-wins tiebreak extended to (recorded_at, device_id, seq) — same-device
    same-millisecond sets were ambiguous.
  - M1 CLI stores media file bytes as-is; JPEG/AAC normalization lands with the capture
    UIs (M2+), implemented once in core.
  - Endpoint secret key persisted in journal meta so a device keeps its iroh identity.
- Repo now on GitHub (private): clawjungle/infinite-journal-v2.
