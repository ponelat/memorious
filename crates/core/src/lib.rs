pub mod api_json;
pub mod crypto;
pub mod enrich;
pub mod event;
pub mod export_md;
pub mod import_v1;
pub mod infra;
pub mod journal;
pub mod media;
pub mod migrate;
pub mod node;
pub mod oauth;
pub mod retention;
pub mod search;
#[cfg(feature = "embeddings")]
pub mod embed_local;
pub mod store;

pub use event::{AudioKind, BlobCrypto, Event, EventKind, MediaKind, Payload};
pub use journal::{Journal, MediaHeld, PeerHoldings};
pub use node::{JournalTicket, Node, SyncReport};
pub use store::{Heads, Store};

/// This build's version — the Cargo package version, so a Nix build (no
/// `.git` in a flake's `src`) gets it for free. Every face (server, desktop,
/// mobile) announces this as a `version_seen` infra event on launch; see
/// `Journal::newer_version_available`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The git commit this build was compiled from (short SHA, `-dirty` suffix
/// for an uncommitted worktree), set by `build.rs`. "unknown" for a build
/// with no `.git` and no `MEMORIOUS_GIT_SHA` override (a raw Nix sandbox
/// build not wired in `flake.nix`) — never fails the build either way.
pub const GIT_SHA: &str = env!("MEMORIOUS_GIT_SHA");

/// UTC date this build was compiled (`YYYY-MM-DD`), set by `build.rs`.
/// Respects a `MEMORIOUS_BUILD_DATE` override for reproducible packaging
/// (Nix); same "unknown" fallback as `GIT_SHA`.
pub const BUILD_DATE: &str = env!("MEMORIOUS_BUILD_DATE");

/// `v0.2.1 (abc1234, 2026-09-21)` — the one line every client shows for
/// itself: the CLI's `--version`, and `version`/`git_sha`/`build_date` in
/// `Node::status_json` (web, desktop, mobile). A compile-time constant
/// (not a `build_info()` `format!`) because clap's `#[command(version =
/// ..)]` needs a `&'static str`.
pub const BUILD_INFO: &str = concat!(
    "v",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("MEMORIOUS_GIT_SHA"),
    ", ",
    env!("MEMORIOUS_BUILD_DATE"),
    ")"
);
