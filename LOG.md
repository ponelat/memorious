# LOG

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
