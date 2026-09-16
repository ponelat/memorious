#!/usr/bin/env bash
# Cross-compile release binaries for ONE target triple via cargo-zigbuild,
# copy them out to dist/<target>/, then delete target/<target> before
# returning. Cross-compile artifacts (musl, iOS, ...) sitting around
# indefinitely is the single biggest cause of this machine's disk filling up
# (docs/BUILD.md) — this script makes "build, extract, clean" the only way to
# do a cross build, instead of ad hoc `cargo zigbuild` calls that pile up
# target/<triple> dirs across sessions. Build one target at a time (call this
# once per triple); do not run it concurrently for different triples.
#
# usage: scripts/cross-build.sh <target-triple> <bin>[:<package>] [<bin>[:<package>] ...]
#   (omit :<package> when the package name equals the bin name)
#
# example: scripts/cross-build.sh x86_64-unknown-linux-musl \
#            memorious:memorious-core memorious-server
set -euo pipefail

if [ $# -lt 2 ]; then
  echo "usage: $0 <target-triple> <bin>[:<package>] [<bin>[:<package>] ...]" >&2
  exit 1
fi

target="$1"; shift
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

rustup target list --installed | grep -q "$target" || rustup target add "$target"

cargo_args=()
bins=()
for spec in "$@"; do
  bin="${spec%%:*}"
  pkg="${spec#*:}"
  [ "$pkg" = "$spec" ] && pkg="$bin"
  bins+=("$bin")
  cargo_args+=(-p "$pkg" --bin "$bin")
done

echo "==> cross-building $target: ${bins[*]}"
cargo zigbuild --release --target "$target" "${cargo_args[@]}"

out="dist/$target"
mkdir -p "$out"
for bin in "${bins[@]}"; do
  cp "target/$target/release/$bin" "$out/$bin"
  echo "==> $out/$bin"
done

size="$(du -sh "target/$target" 2>/dev/null | cut -f1)"
echo "==> cleaning target/$target ($size freed)"
rm -r "target/$target"
