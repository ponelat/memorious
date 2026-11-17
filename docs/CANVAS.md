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
space or middle-drag pans. Right-click opens the tool menu (tools, size, opacity, undo / redo). Image: pick, paste or drop one, drag to move, drag the corner dot to scale, then **place**.
Cmd/Ctrl+Z undoes, with shift (or Ctrl+Y) redoes; options has undo / redo icons and one light/dark button
(it drives the app-wide appearance setting).

**Selection** (`lasso` freehand, `rect` rectangle): the polygon cuts strokes at its boundary, so only the part
inside is selected; images are selected whole when the polygon holds their centre; hidden layers can't be
selected. Drag inside to move, drag the corner dot to scale; duplicate / delete icons sit at the selection's
top-right (Delete, Esc and Cmd+D work too). Edits are events — `move {poly, xf, copy}` and `delete {poly}` —
folded in order over the strokes (`select.ts`), so they undo like anything else. No rotation yet. iPad: touchstart/touchmove are `preventDefault`ed on the canvas —
without that iPadOS swallowed a pencil touch landing within ~0.4 s of the previous one.

## Offset mode (finger)
A mode, entered and left with a **long press** (hold a finger still ~0.5 s on the canvas, with pencil / pen / eraser).
Entering drops a ring — the "mark" — where you pressed. While it exists, a **tap** moves the mark (draws nothing) and a
**drag** starting anywhere moves the tool by the drag's offset from the mark, so the finger isn't covering what's being
drawn or erased. A second long press leaves the mode and clears the mark; so does switching tool. The mark follows
pan/zoom. Mouse and pencil are unaffected. Outside the mode a finger tap leaves a dot and a drag draws under the finger.

## Storage — NOT in the journal yet
First cut is browser-local: strokes/images per month in `localStorage` (`memorious.canvas.events.YYYY-MM`),
image pixels (lossy WebP ≤1024 px) content-addressed in IndexedDB. Nothing syncs and the engine is untouched.
The journal rules still apply to whatever comes next: append-only (undo here is an `active` flag, which would
become a redact event), flat stream, one media format (images would become JPEG blobs through iroh-blobs).
The open design question is the payload: an `Ink` capture carrying a month key + the stroke/image events,
versus one capture per stroke. Decide before touching `crates/core`.

## Not done
Rotating a selection, tilt shading, finger-draw toggle, month rollover while
the tab stays open, journal sync, desktop/iOS faces.
