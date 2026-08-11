#!/usr/bin/env bash
# Build the installable app downloads served at /downloads by the server peer.
# Output: ./downloads/ (gitignored) — point DOWNLOADS_DIR at it.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

OUT="$PWD/downloads"
mkdir -p "$OUT"

# CLI (host arch).
cargo build --release -p journal-core --bin journal
cp target/release/journal "$OUT/journal-cli-macos-arm64"

# Desktop app bundle, zipped for download.
(cd apps/desktop && cargo tauri build 2>&1 | tail -1)
ditto -c -k --keepParent "target/release/bundle/macos/Infinite Journal.app" \
  "$OUT/journal-desktop-macos-arm64.zip"

ls -lh "$OUT"
