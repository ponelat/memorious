# Canvas — platform-neutral behaviour spec

What any Memorious client (web, iOS, Android, desktop, …) must do to offer **the canvas**, so a drawing made on one
device looks the same and behaves the same on every other. Normative words: MUST / SHOULD / MAY.

This is the contract; `docs/CANVAS.md` describes the web client and `docs/CANVAS-SYNC.md` the sync design (read it for the
reasons). The reference implementation is `apps/web/src/canvas/` (`ink.ts` render, `oplog.ts` log, `select.ts` fold). Where this
spec and the reference disagree, fix one of them — they should not drift. The iOS client (`memorious-mobile`) ports the same
three files; the shared **golden vectors** (§9) keep the ports honest.

## 1. Concept

- One **infinite canvas per period**: a **UTC ISO week** (`YYYY-Www`, Monday 00:00 to Sunday 24:00 UTC, keyed by the ISO year; new) or, for canvases made before weekly canvases, a **UTC calendar month** (`YYYY-MM`, legacy, read-only history). Only the *current* week is drawable; earlier periods are shown
  underneath, dimmed to **30 %** opacity, read-only. A user option (*dim earlier*, on by default, remembered per device) shows them at **full brightness** instead. A client MAY let the user pick any other period to show. Every device must agree which week an op belongs to, hence UTC.
- A canvas is **the fold of an append-only log of ops** (§4). Nothing is ever stored as pixels except placed images.
- World coordinates are a plane in "CSS-pixel-like" units, unbounded, origin arbitrary. A **view** is `{x, y, z}`: the world
  point at the viewport's top-left and a zoom (screen px per world unit). Zoom MUST be clamped to **0.1 … 20**.
- Three **layers**, each a view toggle: `pencil`, `pen`, `image`. The eraser has no layer; it acts on pencil and pen.
- Ink colour is the brand orange `#FF5200` (255, 82, 0) on a transparent canvas; the client supplies the paper
  (light or dark). Controls are labels/icons only — no helper text.

## 2. Tools and defaults

| tool | size | opacity | notes |
|---|---|---|---|
| pencil | 4.5 | 0.7 | additive "deposit" with tooth |
| pen | 5 | 1 | solid line, smoothed |
| eraser | 5 | 0.4 | thins *and* fades |
| lasso / rect | – | – | select (§5) |
| image | – | – | place a picture (§6) |

Size and opacity are per tool, user-adjustable (suggested ranges: size 1–40, opacity 0.05–1), remembered per device.
A stroke stores `opacity` only when < 1.

## 3. Ink model and rendering

A stroke is stored as a *few simplified points* plus a *seed*; **every client regenerates the pixels**, so strokes stay sharp
at any zoom and an op is hundreds of bytes. Pixel-exactness across platforms is not required; the same *look* is.

**Point** `(x, y, pressure, tilt)`: world x, y; pressure 0..1 (clamped ≥ 0.02); tilt 0..1 (`1 − altitude/(π/2)`, 0 = upright).
Input without pressure (finger, mouse) synthesises it from speed: first point 0.5, then `clamp(0.95 − 0.28·v, 0.25, 0.95)` with
`v` in world-units/ms, low-pass filtered 0.8/0.2.

**Pen smoothing (capture time):** exponential low-pass on the position (α 0.4) and pressure (0.25) — the *stored* points are the
smoothed ones; on lift, six catch-up steps (α 0.5) toward the raw pen. Pencil and eraser are not smoothed.

**Simplification on lift:** 4-D Ramer-Douglas-Peucker (x, y, and pressure/tilt scaled ×3 as extra dimensions). Tolerance in
world units: pencil 0, pen 0.4, eraser 0.4. Round stored values: x, y to 0.1; pressure, tilt to 0.01.

**Seed:** a random uint32 per stroke, used for a deterministic PRNG (mulberry32) that jitters pencil dab alpha.

**Replay.** Resample the polyline with a Catmull-Rom spline (duplicate the end points) and stamp dabs along it at a spacing
that depends on the tool. Buffers hold **coverage**, not colour; they are rebuilt (replayed in log order) whenever the view
settles or the ops change, then converted to RGBA:

- **Pencil** — an isotropic Gaussian splat (σ = `S·size·0.5·(0.55+0.45p)/2.3` device px, `S` = zoom·dpr), spacing `0.32σ`,
  flow `0.27·(0.1+0.9·p^1.5)`, alpha jitter `0.7+0.6·rand`, times opacity. Deposits add to a *pencil depth* buffer;
  ink alpha = `0.9·d/(d+0.4)·(d<0.15 ? d·6.67 : 1)` (paper tooth strength is 0, so the paper tile is not used).
- **Pen** — anisotropic Gaussian elongated along travel (σ∥ = 1.6σ⊥, σ⊥ = r/1.177, r = `S·size·0.5·(0.4+0.6√p)`), summed into a
  *pen depth* field; the line is where depth crosses 0.5 (smoothstep 0.42–0.58, metaball-style, so wobble merges), plus
  half-amplitude isotropic caps at both ends. Per-pixel **opacity** stacks across strokes but not within one.
- **Eraser** — hard-edged dabs, radius `1.2·size·S`, spacing `0.12r`; each dab multiplies pencil depth, pen depth and pen
  opacity by `1 − a·w`, where `a` is chosen so one pass over a point removes `opacity` in total. It affects only the layers in
  the stroke's `only` list (absent = both).
- Final pixel alpha = pen alpha combined with pencil alpha as `1 − (1−a)(1−b)`; layer toggles gate each term.
- Clients SHOULD cull strokes whose bounds miss the viewport and stamp a peer's newly arrived strokes onto the existing buffers
  when they land at the end of the order, instead of rebuilding.

## 4. The op log

Every drawing action is one **op**, stored as a journal event of kind `ink`: `payload: { type: "ink", period: "YYYY-Www", op, cid, … }` (op fields flat beside `op`; ≤ 512 KB, except `ink_fold`: ≤ 12 MB). Ops never appear in the feed. (Legacy: `infra` events with `infra_kind: "ink"`, `target` = the month, the op under `data`; and `ink` events that carry `month` instead of `period`. All three mean the same.)

| `op` | fields |
|---|---|
| `stroke` | `tool` pencil\|pen\|eraser, `seed`, `size`, `opacity?`, `only?: string[]` (eraser), `pts: [[x,y,p,t],…]` |
| `image` | `hash` (blob), `rect: [x,y,w,h]` |
| `move` | `poly: [[x,y],…]`, `xf: {dx,dy,s,cx,cy}`, `copy?`, `only?`, `ids?` |
| `delete` | `poly`, `only?`, `ids?` |
| `undo` / `redo` | `target` = the `cid` of the op toggled |
| `ink_fold` | `items: [...]` (surviving strokes/images in draw order), `supersedes: [{select:"ink", period, up_to:{device: seq}}, {id}...]` — see *Folding* |

- **`cid`**: a client-generated unique id (UUID) on every op. Undo/redo refer to it (the event id is unknown until stored), and a
  retried push is recognised by it, so nothing applies twice. Ops from older clients without a `cid` use the `event_id`.
- **Order**: confirmed ops sort by (`recorded_at`, `event_id`) — identical on every peer. This device's *unconfirmed* ops go
  after everything confirmed.
- **Undo/redo are per author**: you may only undo your own ops (`device_id` match), most recent active first. The redo stack is
  derived from your own ops (replay: any drawable op clears it, `undo` pushes its target, `redo` removes it), so it survives a
  reload. An op is **active** unless a later `undo` of it isn't matched by a later `redo`. Undo/redo ops themselves sync.
- Unknown `op` values MUST be ignored (not an error) and forwarded untouched.
- Eraser strokes are ordinary `stroke` ops with `tool: "eraser"`.

### Folding (`ink_fold`)

Once a period has ended (plus a grace of ~15 minutes for late-syncing peers) any device may write an **`ink_fold`** op: the period's canvas as the fold leaves it, in one event. `items` are the **surviving strokes and images in draw order**, each stroke with `tool`, `size`, `opacity?`, `seed` and `pts` (clients regenerate the dots, so a fold stays compact). It holds **no eraser strokes** (points an eraser wiped are cut out of the stroke; lightly faded points keep full opacity), **no undo/redo history** and **no per-item times** — anyone who needs the history asks a peer that keeps revisions.

`supersedes` names what the fold makes redundant: the period's ops up to a per-device `seq` bound (`up_to`), and any earlier fold. An op above the bound (drawn offline, synced late) stays live and is folded into the *next* fold, which uses the earlier fold as its base. **Superseding never deletes anything**; each device's retention policy decides whether it keeps the superseded ops or collects them down to stubs (the status page counts them). A reader that has a fold for a period shows the fold's items, then applies only the ops *not* covered by its bound.

Several devices may fold the same period concurrently; the latest fold (by `recorded_at`, then `event_id`) wins and the others are superseded by it.

## 5. Selection (lasso, rect)

`lasso` is freehand (simplify the path, 2-D RDP, tolerance 1.5/zoom world units); `rect` is a rectangle. Both yield a polygon.
Selecting is **non-destructive and lazy**: it computes *pieces*, and only *moving* or *deleting* writes an op.

- **Strokes are cut at the polygon boundary** (interpolating pressure/tilt at the cuts); only the part inside is selected.
  Images are selected whole when the polygon contains their *centre*. Hidden layers cannot be selected. Erasers are not
  selectable ink.
- **Erased ink isn't selected and doesn't come back.** Before a piece is selected or moved, points the erasers visibly wiped
  are cut out: a point counts as erased once the later erasers (visible with that layer) whose path passes within their radius
  (`1.2·size`) leave **< 20 %** of it (each eraser leaves `1 − opacity`). Dense-sample straight segments (step ≥ 0.5, ≈ 0.4× the
  smallest eraser radius) so a 2-point stroke is judged along its length.
- **A selection holds exactly what was selected**: the op carries `ids` (the items it came from) so moving it over other ink never
  picks that up, and moving it again moves only the same pieces. Piece ids: `<id>~<opId>i<n>` for moved pieces,
  `<id>~<opId>o<n>` for the remainder left behind. A moved piece's seed becomes `(seed + 977·(n+1)) mod 2³²`. Ops without `ids` act
  on everything inside the polygon (legacy).
- **`move`** lifts the inside pieces and re-lands them **on top** (after earlier erasers, so those can't touch them), transformed
  `p' = c + d + s·(p − c)` (no rotation); `size` scales by `s`. `copy: true` leaves the originals.
- **`delete`** removes the inside pieces, keeps the remainder.
- `only` = the layers visible when the edit was made; items on other layers are left alone. **An eraser stroke likewise carries
  `only`** and doesn't reach layers that were hidden when it was used.
- UI: drag inside the selection to move, drag the corner handle to scale, duplicate and delete buttons at its top-right; a tap
  outside (or Esc) deselects. Edits are ops, so they undo like anything else. While a drag is live, render the selection as a
  floating layer; write the op on release.

**Wipe** (tool `wipe`, key `A`) is a lasso that **erases everything inside it from the current period and from every earlier period that is showing**. Earlier periods are read-only on the canvas, so the erase is an ordinary `delete` op (polygon, plus `only` = the visible layers) written into *each* period's own log: a fold is the base and the delete folds on top of it, later folds bake it in, and nothing in the journal is removed (the original ink stays in the history that backups keep). The client asks for confirmation first, because an erase in an earlier period cannot be undone from the canvas (undo covers only the current period); in the current period it undoes like any edit.

## 6. Images

`image` op = content-addressed blob (`hash`) + world `rect`; moved/copied/deleted by the same selection rules (atomic, centre-in).
**Pixels do not sync yet** (the op does): a client that lacks the blob MUST draw a neutral placeholder of the right `rect`.
Clients that can place images SHOULD downscale to ~1.3× on-screen size and encode WebP/JPEG at moderate quality before storing.

## 7. Input behaviour

- **Stylus** draws immediately; finger pans/zooms on tablets. The first time a stylus is seen, finger-drawing switches itself off.
  **Phones**: one finger draws, two pan/pinch (a second finger discards the stroke the first started). A **finger** toggle
  overrides.
- Pan: two fingers, space+drag, middle- or right-button drag. Zoom: pinch (about the midpoint), scroll wheel / trackpad about the
  pointer. A right click that doesn't move opens the tool menu.
- **Touch undo/redo**: a two-finger tap = undo, a three-finger tap = redo (discard any stroke a finger had just started).
- **Quick-swap** has two user-selectable modes: *last tool* (toggle between the current and previous tool) or *eraser* (toggle the eraser on, and back to the tool that was active before it). A control (and any hardware gesture the platform exposes — stylus barrel/squeeze/double-tap, keyboard `X`) toggles
  between the current and the previous tool. Holding the stylus still **~0.6 s** also swaps, drawing nothing.
- Keyboard (left hand while the right holds the pencil): `D` pencil, `W` pen, `E` eraser, `S` lasso, `R` rect, `A` wipe, `G` image, `X` swap,
  `C` duplicate / `Q` delete selection, `Esc` deselect, Cmd/Ctrl-Z undo, +Shift redo.
- **Panning/zooming** must stay smooth: while fingers move, show the already-rendered pixels transformed (translate/scale relative to the view they were stamped at); re-stamp when the fingers pause or lift. Pan and pinch recognised together must apply *incremental* deltas to one view, never each restore a private start state (that makes the picture jump).
- **Offset mode (optional, finger)**: long-press (~0.5 s) with a draw tool drops a *mark*; then taps move the mark and drags move
  the tool by the drag's offset from the mark, so the finger doesn't cover the work. Long-press again or switching tool exits.
- Platform gotcha: some OS/browser stacks swallow touches landing < ~0.4 s after the previous one; make sure rapid pencil
  re-landing is delivered (web: `preventDefault` on touchstart/move + `touch-action:none`).

## 8. Chrome (suggested minimum)

Floating, out of the way: **tool picker** (shows the current tool) · **options** (size and opacity with a live sample stroke, light/dark
paper, finger toggle, layer toggles, month list with *other…*, minimap toggle) · **undo / redo** always visible · swap-tool button.
Layers and months live together in their own control (chips: pencil / pen / images, then the months, the current one fixed on), not buried in options. Optional minimap (a user toggle, off by default): a small overview of every shown month fitted to content + viewport, viewport outlined, drag to jump. Zoom −/%/+ (tap % = 100 %).

## 9. Sync and storage

- **Local first.** Apply an op to the in-memory log and persist it to a per-month local cache immediately; the canvas works offline.
- **Push:** debounce ~**0.6 s** after the last op, and on app background / page hide; batches of ≤ 100 ops, in order. The response
  stamps each op with `event_id` + `recorded_at` (the op is then *confirmed*).
- **Pull:** keep a per-month **arrival cursor** (opaque int from the peer — *not* `recorded_at`, because an old op that syncs late
  must still arrive after your last pull). Each sync pulls first, then pushes, so a lost push response is recognised by `cid`.
  Merge dedupes by `cid`. If the peer's `device_id` differs from the cache's, restart from cursor 0.
- **Liveness:** where the host offers a push channel (server-sent events), pull a month when told ink landed; keep a slow poll
  (≈ 60 s) as a safety net; poll fast (≈ 2.5 s current month, 20 s others) when there is no channel. Retry with exponential backoff
  (1 s … 30 s). Where the engine runs in-process (mobile), poll the local journal at ≈ 2.5 s while the canvas is visible and pull
  immediately after each journal sync.
- **Device id:** an author's id is the journal's `device_id`; ops made before it was known are claimed by it once learned.
- Periods are **UTC**. Folding, supersession and stubs: see *Folding* above and `docs/CANVAS-SYNC.md`.

**Engine interface** (identical JSON on every face): `append(month, ops[]) → {events:[{event_id, recorded_at}]}`,
`list(period, after) → {device_id, cursor, events:[{event_id, device_id, seq, recorded_at, data}]}`, `months() → {months:[…]}`.
HTTP: `POST /api/ink/{month}`, `GET /api/ink/{month}?after=`, `GET /api/ink`, `GET /api/ink/stream` (SSE). Mobile (UniFFI):
`ink_append`, `ink_list`, `ink_months`.

## 10. Conformance

`apps/web/tests/canvas-vectors/vectors.json` (generated from the TypeScript reference by `gen.ts`) holds golden cases for the op log
(order, merge, undo/redo, redo-chain) and the fold (`fold`, `piecesIn`, `splitByPoly`, `simplifyPoly`, erased-ink cuts, ids and
seeds). A port passes when it reproduces every case (floats to 1e-9). The renderer is checked by shape, not bit-for-bit: same
mulberry32 sequence and paper tile for a given seed, and coverage buffers that agree with the reference within 1e-3 on the
fixture strokes. When the fold moves into `crates/core` (planned, so snapshots can be built), these vectors become the core's tests
and every shell calls it instead of porting it.

## 11. Not specified yet
Rotation of selections, tilt shading, image pixel sync, month rollover while open, freeze/snapshot/compaction.
