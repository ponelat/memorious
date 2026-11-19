# Infinite month canvas (branch `feat/infinite-canvas`)

One infinite pannable/zoomable canvas per **calendar month**. You only ever draw on the current month's
canvas; earlier months (the previous one by default, any other on request) show through underneath at
30% opacity and are read-only. Layers: pencil, pen, images — each a view toggle, like the month toggles.

Chrome: floating buttons, bottom-left — a quick **tool picker** (shows the current tool), **options**, and **undo** / **redo** (first-class, always visible)
(size + opacity with a live sample of the tool, one light↔dark button, finger, layer toggles, month list with an **other…** picker).

Where: `apps/web/src/canvas/` (`CanvasView.tsx` chrome, `controller.ts` input/replay, `ink.ts` engine,
`store.ts` persistence, `paper.ts` paper tile). Reached from the **canvas** tab in the top bar.

## The ink model (ported from the ink-spike prototype)
- A stroke is a few simplified points `(x, y, pressure, tilt)` + a seed + size + optional opacity. Every client
  regenerates the pixels (so zoom stays sharp and an event is ~hundreds of bytes, not a bitmap).
- Pencil and pen are Gaussian "splats" (anisotropic for the pen: elongated along travel). Pencil adds deposit
  depth → ink through a paper tile; pen sums into a depth field and the line is where depth > 0.5, with a
  per-pixel opacity that stacks across strokes but not within one. Eraser dabs reduce depth *and* opacity.
- Tuned defaults: pencil size 4.5 / opacity 0.7 / flow 0.27, pen size 5 / opacity 1, eraser size 5 / opacity 0.4.
  Tooth and tilt shading are 0 and not exposed.
- Ink is the brand orange; the controls are labels only (no helper text).

## Input
Mouse / Apple Pencil draw. Fingers: on a phone (coarse pointer) one finger draws and two pan/pinch (a second finger
discards the stroke the first one started); the first time a pencil is seen, finger drawing switches itself off so on an
iPad fingers pan. The **finger** button overrides it. Wheel pans, ctrl+wheel (trackpad pinch) zooms,
space, middle- or **right-button drag** pans; a right click that doesn't move opens the tool menu (tools with their
shortcuts, size, opacity, undo / redo).

**Shortcuts** (also shown in the tool list and menu): `P` pencil, `N` pen, `E` eraser, `L` lasso, `R` rect, `I` image;
`X` back to the previous tool (also the swap button next to the tool picker, and a stylus barrel-button press — the
Apple Pencil Pro's *squeeze* is not exposed to web pages by Safari, so it needs the native app); `Space` + drag pan; Cmd/Ctrl+Z undo, +Shift+Z (or Ctrl+Y) redo; Delete, Esc and Cmd/Ctrl+D act on a selection. Image: pick, paste or drop one, drag to move, drag the corner dot to scale, then **place**.
Cmd/Ctrl+Z undoes, with shift (or Ctrl+Y) redoes; options has undo / redo icons and one light/dark button
(it drives the app-wide appearance setting).

**Selection** (`lasso` freehand, `rect` rectangle): the polygon cuts strokes at its boundary, so only the part
inside is selected; images are selected whole when the polygon holds their centre; hidden layers can't be
selected. Drag inside to move, drag the corner dot to scale; duplicate / delete icons sit at the selection's
top-right (Delete, Esc and Cmd+D work too). Edits are events — `move {poly, xf, copy, ids}` and `delete {poly, ids}` —
folded in order over the strokes (`select.ts`), so they undo like anything else. No rotation yet.

A selection holds **exactly the ink that was selected**: the op carries `ids` (the items it came from), so moving it
over other ink never picks that ink up, and moving it again moves only the same pieces (piece ids are `<id>~<op>i<n>`).
Ops without `ids` (older ones) still act on everything inside the polygon. **Erased ink isn't selected and doesn't come
back**: the eraser works on pixels, so before a piece is selected or moved the points the erasers visibly wiped are cut
out of it (a point counts as erased once the erasers passing over it have left < 20 % of it). **An eraser only reaches the
layers that were visible when it was used** (`only` on the stroke), like move / delete.

**Minimap** (options → *minimap*): an overview of the whole canvas — every month on show, erasers included — with the
part you're looking at outlined; click or drag on it to go there. iPad: touchstart/touchmove are `preventDefault`ed on the canvas —
without that iPadOS swallowed a pencil touch landing within ~0.4 s of the previous one.

## Offset mode (finger)
A mode, entered and left with a **long press** (hold a finger still ~0.5 s on the canvas, with pencil / pen / eraser).
Entering drops a ring — the "mark" — where you pressed. While it exists, a **tap** moves the mark (draws nothing) and a
**drag** starting anywhere moves the tool by the drag's offset from the mark, so the finger isn't covering what's being
drawn or erased. A second long press leaves the mode and clears the mark; so does switching tool. The mark follows
pan/zoom. Mouse and pencil are unaffected. Outside the mode a finger tap leaves a dot and a drag draws under the finger.

## Storage and sync
The canvas lives in the journal: every drawing op is an `infra` event (`infra_kind: "ink"`, `target` = the UTC month)
and syncs to every peer — design, op list, undo/redo model and the planned freeze/snapshot/compaction in
**`docs/CANVAS-SYNC.md`**. The browser talks to its server peer over `/api/ink`: ops are applied and cached at once
(localStorage, `memorious.canvas.ops.YYYY-MM`), pushed in batches ~0.6 s after the last one and on page hide, and
the peer is polled every 2.5 s (current month) / 20 s (months shown underneath). Offline strokes queue and flush when
the server is back; drawing made before sync existed is adopted and pushed on first load. Months are **UTC**.
Undo/redo are per author (you undo your own ops). Hosts without `api.ink` (the desktop app, for now) keep the canvas
on that device only.

Tests: `crates/core/tests/ink.rs`, `apps/server/tests/ink.rs`, and the op-log logic in
`apps/web/tests/oplog.test.ts` (`cd apps/web && bun test tests/oplog.test.ts`).

Image pixels do **not** sync yet (the `image` op does; other clients can't draw the picture) — see CANVAS-SYNC.md.

## Not done
Rotating a selection, tilt shading, month rollover while the tab stays open, image pixels in sync, the freeze /
snapshot / compaction phase, and the desktop (Tauri) and iOS faces of `/api/ink`.

## Rendering cost
Every pan / zoom settle replays the month's strokes into the ink buffers, so the work is kept to what changed:
strokes whose bounds miss the view aren't stamped at all; the dimmed months underneath are redrawn only when the view,
layers or one of their logs changed; only the part of the screen the strokes reached is painted; a peer's new strokes
that land at the end of the order are stamped onto what's drawn instead of rebuilding (`stampAppended`); our own push
being confirmed no longer triggers a rebuild unless the order actually changed; months are listed by key scan, not by
parsing every cached log. Measured on 2 × 1500 strokes at DPR 2: idle 30 → 6 ms of main thread per 6 s, one stroke
(draw + push + confirm) 554 → 184 ms, zoom settle 415 → 235 ms, pan settle 421 → 344 ms.
