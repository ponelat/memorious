# Canvas sync: ink in the journal (2026-11-19)

The infinite canvas (`docs/CANVAS.md`) started browser-local. It is meant to be a core part of Memorious, so
what you draw on one client has to reach every other peer. Decisions from the owner (2026-11-19):

- Ink events are sent as you draw, **debounced**, and keep **undo/redo** while the month is open.
- Every ink event belongs to a **month** (UTC).
- After the month has passed it is **frozen**: its ink events are merged into a **month snapshot** and the
  individual events are garbage-collected and **no longer sent in syncs**. Only "the whole" matters afterwards,
  not how it was built up.
- **Exception:** redact events over a frozen month still work and still sync; they are never garbage-collected.

## Phase 1 (built): ink ops as `infra` events

No new event kind and no wire change. `EventKind::Infra` is the journal's open-world envelope (UNDERSTANDING.md
"Infra events"): an ink op is

```
kind: infra   payload: { type: "infra", infra_kind: "ink", target: "YYYY-MM", data: { op: "<name>", cid, ... } }
```

so it rides the ordinary sync, never shows in the feed, and a peer that predates it stores and forwards it
byte for byte. `crates/core/src/ink.rs` validates the envelope (month, a string `op`, ≤ 512 KB) and serves it;
`/api/ink` is the HTTP face; what an op *means* lives in the web client (`apps/web/src/canvas/`).

| `data.op` | fields |
|---|---|
| `stroke` | `tool`, `seed`, `size`, `opacity?`, `pts` (x, y, pressure, tilt) |
| `image` | `hash`, `rect` |
| `move` | `poly`, `xf`, `copy?`, `only?` |
| `delete` | `poly`, `only?` |
| `undo` / `redo` | `target` = the `cid` of the op toggled |

- **`cid`** is a client-generated id on every op. Undo/redo refer to it (an op's event id isn't known until the
  server has it), and a retry after a lost response is recognised by it, so nothing is applied twice.
- **Canvas order** is (`recorded_at`, `event_id`) — shared by every peer, so every peer folds the same way.
- **Polling uses an arrival-order cursor**, not `recorded_at`: an op drawn yesterday on a peer that only synced
  today has an old `recorded_at` but arrives after a reader's last poll and must still be delivered
  (`Store::ink_events`, test `an_op_that_syncs_in_late_is_still_after_the_readers_cursor`).
- **Undo/redo are per author**: you can only undo your own ops; the redo stack is derived from your own ops, so it
  survives a reload. A new op of yours ends the redo chain.
- **Debounce**: the client queues ops locally (local-first, works offline) and pushes a batch ~0.6 s after the last
  one and on page hide.
- **Server-sent events, not polling**: `GET /api/ink/stream` announces `{"month"}` each time ink lands on the peer — drawn
  there or synced in from another peer (`Journal::subscribe_ink`, a broadcast fed from `Store` after the commit) — and the
  client then pulls just that month with its cursor. `ready` (re)connect → catch-up pull of every month shown; `resync` if
  the listener lagged. fetch streaming, not `EventSource`, because the passcode travels in a header. A slow poll (60 s)
  stays as a safety net, the full poll (2.5 s / 20 s) takes over while the stream is down, and the stream reconnects with
  backoff (a 45 s silence — the server sends keep-alives every 20 s — counts as down).

## Phase 2 (dropped 2026-11-26): freeze, snapshot, compact

> **Superseded.** An `ink_fold` (the shipped form of this) was built and then removed: keeping every op is what allows any time-range view of the canvas, and old periods will be made cheap to *render* with a lossy raster cache instead. The text below is kept as history.

A month is **frozen** once its UTC window has passed (plus a grace period for peers that were offline). A
**snapshot** is a merge of that month's ink ops:

```
infra_kind: "ink_snapshot"  target: "YYYY-MM"
data: { covers: { <device_id>: <max seq folded in> }, base?: <earlier snapshot event_id>,
        items: [ { …stroke / image / eraser item as the fold leaves it…, src: <event_id of the op it came from> } ] }
```

The items are what the fold produces (strokes cut by lassos, moves applied, deletions applied, undone ops
dropped, erasers kept in order) — the *whole*, not the build-up — so replaying them draws the same picture.

**Compaction.** The sync protocol needs contiguous per-device `seq` (`Store::insert_remote` rejects a gap), so a
covered ink event is not deleted: it is **compacted** to a stub — same `event_id`/`device_id`/`seq`/`recorded_at`,
`data` replaced by `{ "compacted": <snapshot event_id> }` — and a peer that receives a covered event stores it
already compacted. Heads exchange is untouched; the payload bytes are what stop being kept and sent. An event
with `seq` above `covers[device]` (drawn offline, synced after the snapshot) stays live and folds on top of the
snapshot, so a late peer never loses work.

**Redact over a frozen month.** Snapshot items keep `src`, so a core `redact` of an original ink event id still
resolves after compaction (the stub keeps the id) and simply hides the items with that `src`. Redacts are never
compacted. (A lasso-cut piece can only be redacted as its whole source stroke.)

**Who builds snapshots.** Open: the fold (`select.ts`: polygon cutting, transforms) lives in TypeScript today. The
core's rule is "if logic could live in `crates/core` it must", and iOS/desktop will need the same fold, so the
fold should move into the core (Rust, exposed to the shells) before snapshots are built there. Until then a
snapshot would have to be built by a web client, which is the wrong long-term home.

**Snapshot races.** Two peers may snapshot the same month. A snapshot must be built on the current winning one
(`base`), the winner is the latest by (`recorded_at`, `event_id`), and an event is only compacted once the
*winning* snapshot covers it — so a losing snapshot never causes loss.

## Images (open)

A placed image is a content-addressed blob in the browser's IndexedDB. Its `image` op syncs (rect and hash) but the
pixels do not, so other clients cannot draw it yet. Options: store it as an ordinary photo capture (JPEG, encrypted,
synced by the existing media pipeline — each placed image then also appears in the stream), or inline the small WebP
in the op (no stream entry, but breaks "one media format"). Undecided.

## Not built yet
Phase 2 (everything above), image pixels, the desktop (Tauri) face of `/api/ink`, the iOS face.
