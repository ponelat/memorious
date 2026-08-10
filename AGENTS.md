# Infinite Journal v2 — Agent Guide

Start here. This file orients any AI agent (or human) building the project.

## Read first, in order

1. **`UNDERSTANDING.md`** — the founding document. Every design decision and its reasoning,
   agreed with the owner on 2026-08-10. **It is the source of truth.** If an implementation
   choice contradicts it, the doc wins; if the doc is silent, make the boring choice and note
   it in `LOG.md`.
2. **`MILESTONES.md`** — the build plan with acceptance criteria. Work strictly in milestone
   order; each milestone must leave something working end to end before the next starts.

## One-paragraph brief

A brutally minimalist, append-only, local-first capture device for text, audio, and photos.
One shared Rust core crate (event log, SQLite storage, custom sync protocol over Iroh,
iroh-blobs for media) with four faces: a headless always-on server peer (axum, serves the web
client), a Tauri 2 desktop app, a native SwiftUI iPhone app (UniFFI bindings), and a browser
thin-client of the server. Peers sync directly, union-of-logs, no conflicts, no accounts, no
central authority. There are only four event kinds: capture, redact, token-set, annotation.

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
- **Failing test first.** Work test-driven: before implementing any behavior or bug fix,
  write the test that fails for the right reason, watch it fail, then make it pass.

## Repository layout (target)

```
crates/core/      # the engine: events, storage, sync, enrichment scheduling (all logic here)
apps/server/      # axum HTTP server wrapping core; serves apps/web build; always-on peer
apps/desktop/     # Tauri 2 shell around core + shared web UI
apps/web/         # React + Vite UI, shared by desktop shell and browser client
apps/ios/         # Xcode project, SwiftUI, UniFFI-generated bindings to core
```

Rust side is one Cargo workspace at the repo root. Keep the core deep and the shells thin:
if logic could live in `crates/core`, it must.

## Tech stack & versions (checked 2026-08-10 on crates.io)

- **iroh 1.0.3** (1.0 is out — pin 1.x), **iroh-blobs 0.103.0**, **iroh-gossip 0.101.0**
  (gossip optional; only if live-peer announcement earns its keep). iroh's API churned a lot
  pre-1.0 — trust current docs.rs over training data or old blog posts.
- **rusqlite** with FTS5 for the event log + search.
- **axum** for the server; **uniffi** for Swift bindings; **Tauri 2** for desktop.
- Web: React + Vite + TypeScript. Bun is the JS runtime/package manager on this machine.
- Enrichment (milestone 5): whisper.cpp (or whisper-rs) for audio; OCR via a boring
  well-maintained option (macOS Vision on Apple platforms, tesseract on the server).

## Environment notes

- Dev machine: macOS (Apple Silicon). Local dev domains via Caddy: the local dev domain
  (see the `devhost` skill/CLI on this machine to register the server app's port).
- v1 lives at `~/projects/infinite-journal` (PocketBase + React). Its `GET /api/export`
  is the input for the milestone-6 import tool. Deployed v1: v1 prod.
- Git identity for this repo is set locally (clawjungle). Commit style:
  `type(scope): summary` (see v1's history for tone).
- Keep a `LOG.md` in repo root: dated, terse entries for decisions made while building
  (same habit as v1).

## Definition of done, per change

- `cargo test` green; core logic has tests (the sync protocol especially — two-peer
  convergence tests are non-negotiable).
- Each milestone's acceptance criteria in `MILESTONES.md` demonstrably pass.
- No dead scaffolding: if a shell app exists, it runs.
