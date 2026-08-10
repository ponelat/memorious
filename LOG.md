# LOG

## 2026-08-10 (M2)
- M2 complete: axum server peer (`apps/server`) + React web client (`apps/web`), live at
  the dev app (devhost, port block 4390, release binary; dev journal
  in ./data, gitignored; passcode (redacted) for dev).
- API: auth/check, capture text/photo/audio, feed (cursor pagination), media, redact,
  trash, search, status (incl. pairing ticket). All /api behind bearer passcode; media
  fetched as authed blobs → object URLs (media tags can't send headers).
- Photos normalized to JPEG in core (`media.rs`, image crate); audio: mp4-family kept,
  webm/ogg transcoded to AAC via system ffmpeg (Chrome MediaRecorder → webm; Safari
  records mp4 natively).
- UI: login, capture bar (text/photo/mic), stream with derived day headers, polaroid fan
  for photo runs (30-min gap rule, render-time only), lightbox, trash, sync status, FTS
  search. Adapter seam (`JournalApi`) is the single switch point for the Tauri build.
- Verified end to end in headless Chrome (bdg): login → capture → stream, fan, lightbox.
  Server↔CLI-peer convergence is a cargo test. Phone-browser pass still owner-verified.

## 2026-08-10 (M1)
- M1 complete: core event log + SQLite/FTS5 store, iroh-blobs media store, custom sync
  protocol over iroh 1.0.3, journal-secret pairing tickets, `journal` CLI.
- Demo: two CLI peers on one machine — A served, B joined from A's ticket (2 events +
  1 photo blob), B captured, synced back; both `list` outputs identical. 19 tests green,
  incl. two-peer convergence with media, wrong-secret rejection, interrupted-sync retry.
- Decisions made while building (doc silent, boring choice):
  - Wire frames + ticket payload avoid iroh's own serde (needs deserialize_any; postcard
    rejects it). Frames are length-prefixed JSON; ticket carries an `AddrWire` of strings.
  - Sync auth = keyed blake3 of the journal secret sent in Hello; responder closes on
    mismatch. Possession of secret = trust, per UNDERSTANDING.
  - Both sides fetch *all* referenced-but-missing blobs every sync (not just new ones) —
    heals interrupted transfers for free.
  - Token-set latest-wins tiebreak extended to (recorded_at, device_id, seq) — same-device
    same-millisecond sets were ambiguous.
  - M1 CLI stores media file bytes as-is; JPEG/AAC normalization lands with the capture
    UIs (M2+), implemented once in core.
  - Endpoint secret key persisted in journal meta so a device keeps its iroh identity.
- Repo now on GitHub (private): clawjungle/infinite-journal-v2.
