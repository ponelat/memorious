# Infinite Journal v2

A brutally minimalist, append-only, local-first capture device for text, audio, and
photos. One Rust core (event log, SQLite+FTS5, custom sync over iroh, iroh-blobs media),
four faces: an always-on server peer (axum + React web client), a Tauri 2 desktop app,
a SwiftUI iPhone app (UniFFI), and a CLI. Peers sync directly — union of append-only
logs, no conflicts, no accounts.

Read `UNDERSTANDING.md` (the founding document), then `MILESTONES.md` and `AGENTS.md`.
`LOG.md` records decisions made while building.

## Layout

```
crates/core/       the engine — all logic lives here
crates/mobile/     UniFFI face of core for iOS
apps/server/       axum HTTP peer; serves apps/web build; enrichment sweeper
apps/web/          React + Vite UI (browser client and Tauri shell share it)
apps/desktop/      Tauri 2 shell
apps/ios/          SwiftUI app; ./build.sh makes JournalCore.xcframework
```

## Quick start

```bash
cargo test                                     # whole engine, incl. 2-peer convergence
cargo run -p journal-core --bin journal -- --data /tmp/j init
(cd apps/web && bun install && bun run build)  # web bundle
JOURNAL_DATA=./data PORT=4600 WEB_DIST=apps/web/dist cargo run -p journal-server
```

Dev deployment: registered with devhost as `journal` → the dev app
(passcode set via `journal --data ./data set-passcode …`). Enrichment needs
`brew install whisper-cpp tesseract` + `~/.cache/whisper/ggml-base.bin`.

CLI pairing demo: `journal serve` on one data dir prints a ticket; `journal join <ticket>`
on another creates a second peer; `journal sync <ticket>` converges them.

Import from v1: `journal import-v1 export.json` (idempotent; photos re-fetched).
Markdown mirror: `journal export-md <dir>` (derived, regenerable, never an input).
