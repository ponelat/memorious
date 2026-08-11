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

## Desktop (`apps/desktop`)

```bash
cd apps/desktop && cargo tauri build            # → target/release/bundle/macos/Memorious.app
```

- Tests use tauri's mock runtime: invoke origin must be `tauri://localhost` on macOS and all
  commands are generic over `tauri::Runtime` — keep new commands that way.
- The desktop is **its own peer** (embeds core, own data dir under
  `~/Library/Application Support/com.ponelat.memorious/journal`), never a client of the
  server. `MEMORIOUS_DATA_DIR` env overrides the data dir (used by tests).
- Unsigned/un-notarized: first launch needs right-click → Open.

## iOS (private repo `memorious-mobile`)

The iOS app is proprietary — separate private repo at `~/projects/memorious-mobile`
(github.com/ponelat/memorious-mobile). Its `build.sh` builds the engine from this
repo, located as a sibling checkout by default (`ENGINE_DIR` overrides).

```bash
cd ../memorious-mobile
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
nix build .#memorious-desktop      # Linux build NOT yet verified on a real box
```

- Pinned to **nixpkgs-unstable** — iroh needs rustc ≥ 1.91, 25.05 ships 1.86.
- The desktop derivation overrides `installPhase`: the cargo-tauri hook's own install only
  handles bundles, and we build `--no-bundle`.
- `.app`-domain whois lies about registration; check `dig NS` and the registrar.

## Hosted downloads

```bash
./scripts/make-downloads.sh     # → downloads/ (gitignored), served at /downloads
```

Builds: macOS CLI, static musl Linux CLIs (x86_64 + aarch64 via cargo-zigbuild — run
anywhere including NixOS), and the zipped desktop app. Server lists the directory live; no
restart needed after rebuilding artifacts.
