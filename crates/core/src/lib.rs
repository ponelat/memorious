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
pub mod retention;
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
