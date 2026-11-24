# Building & testing Memorious

Everything here was learned the hard way on the dev Mac (Apple Silicon). Amounts and paths
are real, not aspirational.

## Rust workspace

```bash
cargo test -p memorious-core -p memorious-mobile -p memorious-server   # ~40 tests, fast
cargo test -p memorious-desktop                                        # separate: pulls tauri
```

- **Do not run `cargo test --workspace` casually.** A full debug build of every crate
  (tauri included) is ~20GB of `target/` and has filled this machine's disk three times.
  Test per-package; `cargo clean` between big builds when disk is tight
  (`cargo clean --profile dev` keeps the release artifacts devhost needs).
- **Never pipe long cargo runs through `grep | head`** — the closed pipe can wedge the run
  indefinitely. Redirect to a log file and grep that.
- The dev profile uses `opt-level = 1` (workspace root) because iroh/QUIC are unusable
  unoptimized.
- Renaming a cargo package invalidates every workspace fingerprint → full recompile
  (~35 min). Plan renames accordingly.
- Sync tests spin up real iroh endpoints on loopback; they're CI-runnable but need a network
  stack (no sandbox-blocked sockets).
- rusqlite builds **SQLCipher with vendored OpenSSL** (`bundled-sqlcipher-vendored-openssl`)
  — first compile of `libsqlite3-sys`/`openssl-src` takes a few extra minutes and needs
  `perl` on PATH (macOS has it; the nix flake adds it).

## Web UI (`apps/web`)

```bash
cd apps/web && bun install && bun run build     # tsc + vite → dist/
```

- **Bun, not npm** (npm hangs on this machine). `dist/` is **committed** so nix flake builds
  are self-contained — rebuild and commit it when the UI changes.
- One UI codebase for browser + Tauri. Adapter seam: `src/api/index.ts` picks HTTP or Tauri
  by `__TAURI_INTERNALS__`. Never import adapter internals from components; go through
  `JournalApi`.

## Server (`apps/server`)

```bash
cargo build --release -p memorious-server
```

The release binary is what devhost runs — after any `cargo clean`, rebuild it **before**
`devhost restart memorious` (the running process survives on a deleted inode; a restart
without the binary present fails).

## Native Linux desktop (`apps/desktop/linux`)

```bash
nix develop
cargo run -p memorious-linux --bin memorious-desktop
cargo test -p memorious-linux --no-default-features --test journal
cargo test -p memorious-linux --test native_ui -- --ignored  # needs a display
```

System dependencies: GTK ≥ 4.10, libadwaita ≥ 1.5, pkg-config, Perl and a C
toolchain. Debian/Ubuntu: `libgtk-4-dev libadwaita-1-dev pkg-config perl build-essential`.
The Linux flake desktop target builds this native Rust application without WebKit,
Tauri, Bun, or the web bundle. GTK media playback needs the platform’s GTK media
backend and GStreamer codecs for AAC/MP4.

The desktop embeds its own Rust node and opens the same encrypted journal as the
previous Linux desktop: `$XDG_DATA_HOME/app.memorious/journal` (normally
`~/.local/share/app.memorious/journal`). `MEMORIOUS_DATA_DIR` overrides it.
Passwords may be cached in the Linux keyring; a missing keyring leaves manual unlock
available. Override directories never share the default keyring credential.
The journal lock prevents a second desktop or daemon from opening the same data.

## Existing macOS/Windows desktop (`apps/desktop/src-tauri`)

```bash
cd apps/desktop && cargo tauri build
```

The Tauri shell remains for these platforms during the native transition. Tests use
Tauri’s mock runtime. The macOS journal lives under
`~/Library/Application Support/app.memorious/journal`.

## iOS (private repo `memorious-mobile`)

The iOS app is proprietary — separate private repo (github.com/ponelat/memorious-mobile),
checked out as a sibling of this repo. Its `ios/build.sh` builds the engine from this
repo, located as `../../memorious` by default (`ENGINE_DIR` overrides).

```bash
cd ../memorious-mobile/ios
./build.sh --device             # rust for sim+device, uniffi Swift bindings, XCFramework
xcodegen generate
xcodebuild -project Memorious.xcodeproj -scheme Memorious \
  -destination 'generic/platform=iOS' -derivedDataPath build build
```

- Bindings are generated in library mode from a **host** dylib; regenerate whenever
  `crates/mobile`'s exported surface changes.
- `project.yml` (xcodegen) is the source of truth — the `.xcodeproj` is generated and
  gitignored. Linker needs `-framework SystemConfiguration` (iroh's netdev).
- Simulator UI tests: `xcodebuild ... test -only-testing:MemoriousUITests/...`. The
  join-by-ticket test takes a live host peer's ticket via `TEST_RUNNER_TEST_JOIN_TICKET`.
- Signing/deploy specifics are in the owner's private ops runbook.

## Nix flake

```bash
nix build .#memorious-cli          # verified on aarch64-darwin
nix build .#memorious-server
nix build .#memorious-desktop      # native Linux GTK 4/libadwaita desktop
```

- Pinned to **nixpkgs-unstable** — iroh needs rustc ≥ 1.91, 25.05 ships 1.86.
- The desktop derivation selects GTK 4/libadwaita on Linux and Tauri on macOS.
  It installs a desktop entry whose app id matches `app.memorious`, plus hicolor icons.
- `.app`-domain whois lies about registration; check `dig NS` and the registrar.

## Hosted downloads

```bash
./scripts/make-downloads.sh     # → downloads/ (gitignored), served at /downloads
```

Builds: macOS CLI, static musl Linux CLIs (x86_64 + aarch64 via cargo-zigbuild — run
anywhere including NixOS), and the zipped desktop app. Server lists the directory live; no
restart needed after rebuilding artifacts.

The Linux musl CLI here is also what an eventual AUR `memorious-bin` package would install
directly — see `docs/AUR.md` for the packaging plan.

## Cross-compiling (musl, EC2 deploys, etc.)

```bash
./scripts/cross-build.sh <target-triple> <bin>[:<package>] [<bin>[:<package>] ...]
# e.g.
./scripts/cross-build.sh x86_64-unknown-linux-musl memorious:memorious-core memorious-server
```

Always go through this script for a cross target rather than calling `cargo zigbuild`
directly. It builds one target triple at a time, copies the resulting binaries to
`dist/<target>/` (gitignored, survives the cleanup), then deletes `target/<target>`
immediately — a `target/x86_64-unknown-linux-musl` or `target/aarch64-apple-ios*` left
sitting around after a deploy is exactly what has filled this machine's disk before.
`make-downloads.sh`'s Linux CLI builds already go through it; do the same for one-off
deploys (e.g. the EC2 peers in the owner's private ops runbook).

## Linux background capture

The native toolbar’s Settings button opens **Background & PTT**, which configures
**Run in the background** and **Enable push-to-talk**. Background mode hands the same directory and identity to
this executable’s no-window daemon personality. Reopening the app attaches to the
owner-only socket. Closing the window keeps that daemon and its PTT worker running;
turning background mode off validates the master password before transferring ownership
back. A background process also exports a StatusNotifierItem/DBusMenu indicator
(Open Memorious, start/stop recording, stop background). GNOME needs AppIndicator
support to display standard tray icons.

The Python worker remains `apps/desktop/ptt/memorious-ptt.py`, installed alongside the
native binary as `memorious-ptt`. Runtime dependencies are Python with `evdev` and
`requests`, plus PipeWire’s `pw-record`. The flake bundles these. The global key
(default `KEY_RIGHTCTRL`) requires access to the relevant `/dev/input` devices;
the toolbar and tray controls use an owner-only socket and work without that access.
The worker’s local settings retain the previous desktop’s filename and schema, and
API keys are never returned in status. Recording and pending WAVs are owner-only;
failed transcription/delivery retains the queue for retry. No microphone is opened
until a recording control or the configured key is used.

```bash
python3 apps/desktop/ptt/test_worker.py
cargo test -p memorious-linux --test background --test tray
cargo test -p memorious-linux --test native_ui -- --ignored --test-threads=1
```

### Local or remote transcription

The native settings dialog selects local multilingual **Whisper Small** or remote
API transcription. The bundled worker and desktop voice-file importer use the same
provider and language setting (`auto`, or a code such as `af`, `en`, `es`). Local
mode uses `whisper-cli` and never invokes remote transcript cleanup or falls back
to the API. New configurations default to local; existing configurations without
these fields keep remote transcription and automatic language detection.

**Download local model** fetches the complete 487,601,967-byte full-precision GGML
model from the upstream whisper.cpp model repository. The URL is pinned to a
revision and the file is checked against SHA-256 before an atomic rename. The
shared device cache is `$XDG_CACHE_HOME/memorious/transcription/whisper-small`;
`MEMORIOUS_MODEL_DIR` overrides the cache root in tests. Interrupted downloads can
resume; partial files never count as installed. The model is outside the journal
and is never synced. This size is the model download; inference RAM is separate.

Voice files are captured and synced first, then transcribed off the GUI thread.
Their text becomes an annotation. Music is never passed through this pipeline.
The runtime needs `whisper-cpp` and `ffmpeg`, both bundled by the Linux flake.
No model weights are included in the application package or downloaded implicitly
by inference.

```bash
python3 apps/desktop/ptt/test_models.py
# Model-management commands supported by the bundled helper:
memorious-ptt --model-status
memorious-ptt --download-model
```

### Native peer troubleshooting

In **Settings → Peers & sync**, leave the ticket blank to sync all known peers. A new
address can be supplied with a fresh ticket from the other device. **Check
connections** runs bounded per-peer probes; sync results report partial failures
and update the displayed holdings. A device cannot sync with its own ticket.

The same dialog exposes relay mode, custom relay URLs and public address lookup.
Changes are local to this peer and apply after its journal restarts. When running
in the background, closing the window alone does not restart the journal: use
**Stop background journal** from the tray, then reopen the app. Direct-only mode
may prevent connections across networks even while a remote peer is online.

### Native stream presentation checks

The native UI suite includes a throwaway text/audio feed that checks selectable
monospaced text, seven-line expansion (including paragraphs and Unicode wrapping),
window resizing, and the scroll-only timestamps/rules with their idle delay.
Set `MEMORIOUS_UI_SCREENSHOT=/tmp/memorious-stream.png` when running `native_ui`
to save that fixture's rendered window. The fixture creates no journal and opens
no media. Run display checks serially as shown above.

### Native audio capture and appearance

The composer microphone records a voice audio entry without requiring background
mode or PTT. Click again to stop and save; **Cancel recording** discards it.
The timer appears only after PipeWire supplies audio. A failed save keeps the
encoded recording in memory for retry, and closing the window requires saving or
cancelling first. PCM and AAC/m4a use anonymous memory files rather than plaintext
recording paths. Audio is encrypted, captured and synced before transcription.
The foreground recorder needs `pw-record` and `ffmpeg`; the Linux package adds
both to the app's runtime PATH.

PTT remains dictation. Its worker uses `pw-play` (from the existing PipeWire
runtime) for a short start beep after the microphone supplies samples. A failed
microphone start does not beep. Playback failure does not interrupt capture.

**Settings → Appearance → Dark mode** selects dark/light appearance and saves it under
`$XDG_CONFIG_HOME/memorious/appearance` (normally `~/.config/memorious/appearance`).
Before an explicit choice the app follows the system. Appearance is device-local.

The native UI checks include isolated start/cancel/save/encrypted-reopen flows
with simulated microphone input, plus composer and appearance persistence checks.
The recorder unit tests encode real AAC with ffmpeg and check process cleanup.

```bash
cargo test -p memorious-linux --lib
cargo test -p memorious-linux --test native_ui -- --ignored --test-threads=1
python3 apps/desktop/ptt/test_worker.py
```

### Unified native settings

One Settings icon opens Appearance, Background & PTT, and Peers & sync in the same
panel. A native `AdwNavigationSplitView` shows a sidebar at desktop widths and
page navigation with a Back button below 600sp. Each pane's `AdwToolbarView`
keeps its header and close button outside the scrolling section content.

Graphical checks cover section navigation, persisted appearance, narrow layouts
and Back navigation, plus scrolling both long sections and clicking the visible
close button. `MEMORIOUS_UI_SCREENSHOT` can also save these panel fixtures.

For repeatable checks independent of the live desktop, use an isolated display:

```bash
GDK_BACKEND=x11 GSK_RENDERER=cairo xvfb-run -a \
  cargo test -p memorious-linux --test native_ui -- --ignored --test-threads=1
```

### Native canvas and current event schema

The header's **Canvas** button opens a native GTK drawing surface. Pencil, pen,
eraser, lasso, rectangle and timeline edits use the shared Rust canvas model and
coverage renderer. Mouse/stylus drawing, pressure, pan, scroll/pinch zoom, layers,
weekly periods and legacy months use the same operation JSON as the browser.
Completed gestures are written straight into the encrypted journal; the ordinary
peer sync sends them. The canvas polls the local journal while visible, including
when the owner is a background daemon. Saving failures retain the pending gestures
in memory and retry; closing waits for them to save. Current-week undo/redo is per
author. Timeline edits may reach visible earlier periods; deleting there asks for
confirmation. Image ops without locally available pixels show neutral rectangles.

The shared reader applies the latest `ink_fold` base, skips operations covered by
its per-device sequence bounds, and keeps late arrivals above those bounds. Unknown
ops are ignored by the renderer and preserved by the journal. No migration is
needed to read legacy ink events. The coverage fixture is generated from the web
renderer and checks pencil, pen, opacity stacking and layer-specific erasure.

**Settings → Peers & sync** also exposes required/optional peers, retention presets,
collected-history counts and isolation. Isolation immediately stops sync and forgets
peers; restart the journal for its new address, then end isolation and pair again.
In background mode, stop the journal from the tray before reopening it.

```bash
cargo test -p memorious-core --test canvas_vectors --test canvas_render \
  --test canvas_input --test canvas_session
cargo test -p memorious-linux --no-default-features --test features --test canvas
cargo test -p memorious-linux --test background
ADW_DISABLE_PORTAL=1 GDK_DEBUG=no-portals GDK_BACKEND=x11 GSK_RENDERER=cairo xvfb-run -a \
  cargo test -p memorious-linux --test native_ui -- --ignored --test-threads=1
```

First-run pairing offers **Write-only peer** for a capture surface that pushes its
own notes without pulling history. Like the CLI's write-only join, it cannot check
the network's password proof without pulling: use the same password as the other
peers. **Export lossless archive…** writes a new ZIP containing event JSONL, schemas,
a manifest and locally available decrypted media; choosing an existing filename
fails rather than overwriting it.
