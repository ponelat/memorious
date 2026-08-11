# Memorious

A brutally minimalist, append-only, local-first capture device for text, audio, and
photos. One Rust core (event log, SQLite+FTS5, custom sync over iroh, iroh-blobs media),
four faces: an always-on server peer (axum + React web client), a Tauri 2 desktop app,
a SwiftUI iPhone app (UniFFI), and a CLI. Peers sync directly — union of append-only
logs, no conflicts, no accounts.

Read `UNDERSTANDING.md` (the founding document) and `AGENTS.md`, then `docs/BUILD.md` and
`docs/DEPLOY.md` for how to work on it. `apps/landing/CONTEXT.md` covers memorious.app.
`LOG.md` records decisions made while building; `MILESTONES.md` is historical.

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
cargo run -p memorious-core --bin memorious -- --data /tmp/j init
(cd apps/web && bun install && bun run build)  # web bundle
MEMORIOUS_DATA=./data PORT=4600 WEB_DIST=apps/web/dist cargo run -p memorious-server
```

Dev deployment: registered with devhost as `journal` → the dev app
(passcode set via `memorious --data ./data set-passcode …`). Enrichment needs
`brew install whisper-cpp tesseract` + `~/.cache/whisper/ggml-base.bin`.

CLI pairing demo: `memorious serve` on one data dir prints a ticket; `memorious join <ticket>`
on another creates a second peer; `memorious sync <ticket>` converges them.

Import from v1: `memorious import-v1 export.json` (idempotent; photos re-fetched).
Markdown mirror: `memorious export-md <dir>` (derived, regenerable, never an input).

## Downloads

`scripts/make-downloads.sh` builds installable artifacts into `./downloads/`
(served by the server at `/downloads`, listed in the web client's sync view):
macOS CLI + desktop .app zip, and static musl Linux CLIs (x86_64 + aarch64)
via cargo-zigbuild — those run on any Linux, NixOS included.

## NixOS / Nix

The flake builds everything from source (repo is private — use ssh fetch):

```bash
nix run  'git+ssh://git@github.com/clawjungle/infinite-journal-v2'             # CLI
nix run  'git+ssh://git@github.com/clawjungle/infinite-journal-v2#desktop'     # desktop app
nix build 'git+ssh://git@github.com/clawjungle/infinite-journal-v2#memorious-server'
```

Or from a checkout: `nix build .#journal-cli|memorious-server|memorious-desktop`.
The desktop links system webkitgtk 4.1/GTK on Linux, so there is deliberately no
portable Linux desktop binary — the flake (or the static CLI) is the path.
Pin note: nixpkgs-unstable, because iroh needs rustc ≥ 1.91.
