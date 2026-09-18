# AUR packaging source

Working source for the two AUR `-bin` packages described in `../../docs/AUR.md`. This
directory is what gets copied into each package's own AUR git repo
(`ssh://aur@aur.archlinux.org/<pkgname>.git`) — it is not itself an AUR repo.

## memorious-bin — CLI (done, tested)

Tracks the static musl Linux CLI binaries attached to GitHub Releases
(`https://github.com/ponelat/memorious/releases`). `PKGBUILD` + `.SRCINFO` here were
verified end-to-end against the v0.2.0 release with `makepkg -si` in a clean
`archlinux:latest` container (namcap-clean apart from expected `-bin`/static-binary
warnings: literal arch-suffixed source array names, missing PIE/RELRO on the prebuilt
binary — neither is fixable from the PKGBUILD side).

To publish or update:

```bash
git clone ssh://aur@aur.archlinux.org/memorious-bin.git
cp packaging/aur/memorious-bin/{PKGBUILD,.SRCINFO} memorious-bin/
cd memorious-bin
git add PKGBUILD .SRCINFO
git commit -m "memorious-bin 0.2.0-1"
git push
```

Needs an AUR account with an SSH key registered first — see `../../docs/AUR.md`.

## memorious-desktop-bin — GUI (blocked on a Linux tarball)

`PKGBUILD` here is a template with `pkgver`/`sha256sums_x86_64` TODOs — it can't be
finalized until a `memorious-desktop-linux-x86_64.tar.gz` exists as a GitHub release
asset. Build it on a real Linux box (this Mac is aarch64-darwin; the flake's desktop
package needs webkitgtk/gtk3, which don't cross-compile from macOS):

```bash
# On a Linux box (e.g. the NixOS laptop) with this repo checked out:
nix build .#memorious-desktop
mkdir memorious-desktop-linux-x86_64
cp result/bin/memorious-desktop memorious-desktop-linux-x86_64/
cp result/share/applications/memorious-desktop.desktop memorious-desktop-linux-x86_64/
cp -r result/share/icons memorious-desktop-linux-x86_64/icons
tar czf memorious-desktop-linux-x86_64.tar.gz memorious-desktop-linux-x86_64
sha256sum memorious-desktop-linux-x86_64.tar.gz

# Attach it to the matching GitHub release (bump the tag first if this is past v0.2.0):
gh release upload v0.2.0 memorious-desktop-linux-x86_64.tar.gz -R ponelat/memorious
```

Then fill in `pkgver` (if a new tag was needed) and the real sha256 in
`memorious-desktop-bin/PKGBUILD`, test with `makepkg -si` in an Arch container the same
way `memorious-bin` was tested, regenerate `.SRCINFO`, and publish the same way as above
with `memorious-desktop-bin` instead.
