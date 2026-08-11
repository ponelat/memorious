# Memorious — Agent Guide

Start here. This file orients any AI agent (or human) working on the project.

## Read first

1. **`UNDERSTANDING.md`** — the founding document (2026-08-10). Every design decision and its
   reasoning. **It is the source of truth.** If an implementation choice contradicts it, the doc
   wins; if the doc is silent, make the boring choice and note it in `LOG.md`.
2. **`docs/BUILD.md`** — how to build and test every part.
3. `MILESTONES.md` is historical (all six milestones shipped 2026-08-10); `LOG.md` is the
   dated decision record — keep appending to it.

Deployment/ops details are deliberately not in this repo (owner-private runbook).

## One-paragraph brief

The product is **Memorious** (named 2026-08-11; public site memorious.app). "Journal" remains
the domain noun for the data structure; Memorious is the product/brand.

A brutally minimalist, append-only, local-first capture device for text, audio, and photos.
One shared Rust core crate (event log, SQLite storage, custom sync protocol over iroh,
iroh-blobs for media) with four faces: a headless always-on server peer (axum, serves the web
client), a Tauri 2 desktop app, a native SwiftUI iPhone app (UniFFI bindings), and a browser
thin-client of the server. Peers sync directly, union-of-logs, no conflicts, no accounts, no
central authority. There are only four event kinds: capture, redact, token-set, annotation.
Everything is encrypted at rest under one master password per journal (SQLCipher database,
per-blob AEAD sealed before ingest) — see UNDERSTANDING.md §"Encryption at rest" before
touching storage, sync, or key handling.

## Hard rules (from the owner — do not renegotiate)

- **Append-only.** No event is ever mutated or deleted; redaction is an event.
- **Flat stream.** No conversations, chunks, tags, captions, or stored grouping. Grouping is
  derived at render time only.
- **No iroh-docs.** The sync protocol is our own (heads exchange → send missing events).
- **Capture syncs immediately.** Enrichment (transcription/OCR) never delays capture.
- **No accounts.** Trust = possession of the journal secret (QR/ticket pairing). Browser auth
  = one active bearer passcode (hash stored in a token-set event, latest wins).
- **One media format each:** AAC/m4a audio, JPEG photos. No originals kept.
- **Minimalism is a feature.** When in doubt, leave it out.
- **Failing test first.** Before implementing any behavior or bug fix, write the test that
  fails for the right reason, watch it fail, then make it pass.

## Repository layout

```
crates/core/       the engine — events, storage, sync, enrichment scheduling; ALL logic here
crates/mobile/     UniFFI face of core for iOS (JSON strings + bytes over blocking calls)
apps/server/       axum HTTP peer; serves apps/web build; enrichment sweeper; /downloads
apps/web/          React + Vite UI shared by the browser client and the Tauri shell
apps/desktop/      Tauri 2 shell (its own peer, embeds core — never a client of the server)
docs/              build + deploy context
scripts/           make-downloads.sh (hosted app builds)
```

This repo is open source (MIT OR Apache-2.0; see README §License). The native iOS app is
proprietary and lives in the separate **private** repo github.com/ponelat/memorious-mobile,
checked out as a sibling of this repo; it builds `MemoriousCore.xcframework` from this
repo's `crates/mobile` via its own `ios/build.sh` (default engine path: `../../memorious`).
Never move paid-app code into this repo.

Keep the core deep and the shells thin: if logic could live in `crates/core`, it must.
The one UI seam is `apps/web/src/api/types.ts` (`JournalApi`) — browser implements it with
HTTP, desktop with Tauri commands. The wire shape for entries is
`crates/core/src/api_json.rs`, shared by server, desktop, and mobile.

## Versioning rules that bite

- **Protocol identifiers travel together.** `SYNC_ALPN`, the ticket prefix, and
  `AUTH_CONTEXT` live in `crates/core/src/node.rs`. Changing any of them strands every
  deployed peer — only do it with a plan to upgrade server, desktop, and phone in one go.
- **The iOS bundle id predates the rename** (details in the mobile repo). Don't "fix" it
  casually — it requires a live Xcode Apple ID session and orphans the app's on-phone data.
- iroh is pinned 1.x; its pre-1.0 API differs wildly from training data — trust docs.rs and
  the vendored sources in `~/.cargo/registry`. Two local gotchas: iroh's `EndpointAddr` serde
  needs `deserialize_any` (postcard can't — wire frames are JSON, tickets use `AddrWire`),
  and nixpkgs must be new enough for rustc ≥ 1.91.

## Environment notes

- Dev machine: macOS (Apple Silicon). Dev hosting via the owner's local `devhost` CLI.
- v1 is the PocketBase predecessor; its `GET /api/export` feeds `memorious import-v1`.
  The owner's real import sits in `./data-v1-import/` (gitignored).
- Git identity: clawjungle. Commit style: `type(scope): summary`.
- Keep `LOG.md` current: dated, terse entries for decisions made while building.

## Definition of done, per change

- `cargo test` green (run per-package — see docs/BUILD.md for the disk story); core logic has
  tests, the sync protocol especially.
- If a shell app exists, it runs. No dead scaffolding.
- Deployed surfaces updated when behavior changes (owner's private ops runbook).
