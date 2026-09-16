# Publishing to the AUR

Plan for getting Memorious onto the Arch User Repository, so Arch/Omarchy users can find
and install it via `yay`/`paru` (or Omarchy's own app-adding flow, which is backed by the
same tooling). Not started yet — this is the checklist before any of it exists.

## What's needed

- **An AUR account** — free registration at aur.archlinux.org.
- **An SSH key registered to that account** — AUR packages are pushed as git repos over
  SSH (`ssh://aur@aur.archlinux.org/<pkgname>.git`); a normal `id_ed25519`-style key added
  in the account's SSH Public Key field.
- **A namespace check** — confirm `memorious` / `memorious-bin` aren't already taken
  (search aur.archlinux.org before creating either repo; AUR package names are first-come).
- **A `PKGBUILD` per package** — the Arch build recipe: `pkgname`, `pkgver`, `pkgrel`,
  `arch=('x86_64')`, `url`, `license=('MIT' 'Apache-2.0')` (matches this repo's dual
  license), `source=()` + `sha256sums=()`, and a `package()` function.
- **A `.SRCINFO`** alongside it — generated metadata (`makepkg --printsrcinfo > .SRCINFO`)
  that AUR's web UI and `yay`/`paru` actually parse. Must be regenerated every time
  `PKGBUILD` changes, or the AUR page silently goes stale relative to the real recipe.
- **A way to test before pushing** — `makepkg -si` needs an actual Arch system (or an Arch
  container: `docker run --rm -it archlinux` + `pacman -Sy base-devel`). Don't push an
  untested `PKGBUILD` — it breaks the page for everyone until fixed.
- **An ongoing maintenance commitment** — every release means bumping `pkgver`, updating
  `sha256sums`, regenerating `.SRCINFO`, and pushing. An AUR package that falls behind gets
  flagged out-of-date by users, or eventually orphaned. This is the real cost of AUR
  compared to a plain download link — see the earlier discussion in this session about
  self-hosted builds being simpler than a full binary-cache setup for the same reason.

## Two packages, not one

- **`memorious-bin`** — the CLI. Ready to package *today*: `make-downloads.sh` already
  builds and hosts a static musl `memorious-cli-linux-x86_64` (no runtime deps at all,
  runs on any glibc or musl distro). The `PKGBUILD` just downloads that URL and installs
  it to `/usr/bin/memorious`.
- **`memorious-desktop-bin`** — the GUI app. **Blocked**: there's currently no Linux
  desktop build hosted anywhere (`make-downloads.sh` only bundles the desktop app for
  macOS). The Nix flake already knows how to build one (`memorious-desktop` package +
  `.desktop` entry + icons, `flake.nix`), and the immediate prerequisite — a plain
  downloadable `memorious-desktop-linux-x86_64.tar.gz`, discussed earlier this session —
  needs to exist first, on a real Linux build host (this Mac is aarch64-darwin). This
  package would declare `depends=('webkitgtk-4.1' 'gtk3' 'libsoup3' 'glib-networking')`
  since a `-bin` package can't build its own runtime deps.

Ship `memorious-bin` first — it has no blockers. `memorious-desktop-bin` follows once the
Linux desktop tarball exists.

## Steps (once ready)

1. `git clone ssh://aur@aur.archlinux.org/memorious-bin.git` (empty repo, created
   automatically by AUR on first push to a new name).
2. Write `PKGBUILD`, run `makepkg -si` locally to confirm it actually installs and runs.
3. `makepkg --printsrcinfo > .SRCINFO`.
4. `git add PKGBUILD .SRCINFO && git commit && git push`.
5. Repeat 2–4 for `memorious-desktop-bin` once its tarball exists.
6. Each release: bump `pkgver`/`sha256sums`, regenerate `.SRCINFO`, push. Consider a small
   script (`aur/bump.sh`?) once this repo has its own release-tagging flow — there isn't
   one yet (no `.github/`, no CI, no versioned release artifacts — see `docs/BUILD.md`).

## Open decisions

- Package naming: `memorious-bin` vs `memorious` (source build) — `-bin` is the plan above
  since it reuses the already-hosted static binary and needs no build step for users.
- Whether to also do a source-build `memorious` package later for distros/policies that
  prefer building from source over installing a binary — not planned for now.
