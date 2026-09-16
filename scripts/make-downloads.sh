#!/usr/bin/env bash
# Build the installable app downloads served at /downloads by the server peer.
# Output: ./downloads/ (gitignored) — point DOWNLOADS_DIR at it.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

OUT="$PWD/downloads"
mkdir -p "$OUT"

# License attribution must travel with the binaries (SQLCipher/OpenSSL are
# statically linked): serve the notices next to every download.
cp THIRD-PARTY-NOTICES.md "$OUT/"

# CLI (host arch).
cargo build --release -p memorious-core --bin memorious
cp target/release/memorious "$OUT/memorious-cli-macos-arm64"

# Static Linux CLIs (musl, run anywhere incl. NixOS) via cargo-zigbuild. One
# target at a time, cleaned up immediately after (scripts/cross-build.sh) —
# leaving both target/<triple> dirs around is what fills this machine's disk.
if command -v cargo-zigbuild >/dev/null; then
  ./scripts/cross-build.sh x86_64-unknown-linux-musl memorious:memorious-core
  cp dist/x86_64-unknown-linux-musl/memorious "$OUT/memorious-cli-linux-x86_64"
  ./scripts/cross-build.sh aarch64-unknown-linux-musl memorious:memorious-core
  cp dist/aarch64-unknown-linux-musl/memorious "$OUT/memorious-cli-linux-aarch64"
else
  echo "cargo-zigbuild not installed — skipping Linux CLI builds"
fi

# Desktop app bundle, zipped for download — notices ride inside the zip.
(cd apps/desktop && cargo tauri build 2>&1 | tail -1)
STAGE="$(mktemp -d)"
ditto "target/release/bundle/macos/Memorious.app" "$STAGE/Memorious.app"
cp THIRD-PARTY-NOTICES.md "$STAGE/"
ditto -c -k "$STAGE" "$OUT/memorious-desktop-macos-arm64.zip"
rm -r "$STAGE"

ls -lh "$OUT"
