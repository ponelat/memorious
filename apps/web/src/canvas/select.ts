/** Selection maths + the event fold. Ported from the ink-spike prototype (minus rotation).
 *  A month canvas is the fold of its append-only events: strokes and images append; a `delete` removes what is
 *  inside a polygon; a `move` lifts what is inside a polygon and re-lands it (scaled / shifted) on top. Strokes
 *  are cut at the polygon boundary, so a lasso through a line moves only the part inside; images are atomic
 *  (selected whole when the polygon holds their centre). */
import type { Ev, ImageItem, Item, Layers, Poly, Pt, StrokeItem, Xf } from './ink'

export const layerOf = (it: Item): keyof Layers => (it.tool === 'image' ? 'image' : it.tool === 'pen' ? 'pen' : 'pencil')

/** Layers an edit touched, or undefined when all are visible (so every layer is affected). */
export const onlyList = (l: Layers): string[] | undefined => {
  const v = (Object.keys(l) as (keyof Layers)[]).filter((k) => l[k])
  return v.length === 3 ? undefined : v
}

export function pip(poly: Poly, x: number, y: number): boolean {
  let c = false
  for (let i = 0, j = poly.length - 1; i < poly.length; j = i++) {
    const a = poly[i]
    const b = poly[j]
    if (a[1] > y !== b[1] > y && x < ((b[0] - a[0]) * (y - a[1])) / (b[1] - a[1]) + a[0]) c = !c
  }
  return c
}

const lerpPt = (a: Pt, b: Pt, t: number): Pt => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, a[3] + (b[3] - a[3]) * t]

function crossings(a: Pt, b: Pt, poly: Poly): number[] {
  const ts: number[] = []
  const rx = b[0] - a[0]
  const ry = b[1] - a[1]
  for (let i = 0, n = poly.length; i < n; i++) {
    const c = poly[i]
    const d = poly[(i + 1) % n]
    const sx = d[0] - c[0]
    const sy = d[1] - c[1]
    const den = rx * sy - ry * sx
    if (Math.abs(den) < 1e-12) continue
    const t = ((c[0] - a[0]) * sy - (c[1] - a[1]) * sx) / den
    const u = ((c[0] - a[0]) * ry - (c[1] - a[1]) * rx) / den
    if (t > 1e-9 && t < 1 - 1e-9 && u >= 0 && u < 1) ts.push(t)
  }
  return ts.sort((p, q) => p - q)
}

/** Split a polyline at the polygon boundary: pieces inside / outside, with interpolated points (pressure, tilt too) at the cuts. */
export function splitByPoly(pts: Pt[], poly: Poly): { ins: Pt[][]; outs: Pt[][] } {
  const ins: Pt[][] = []
  const outs: Pt[][] = []
  let cur: Pt[] | null = null
  let curIn = false
  const flush = () => {
    if (cur && cur.length) (curIn ? ins : outs).push(cur)
    cur = null
  }
  if (pts.length === 1) {
    ;(pip(poly, pts[0][0], pts[0][1]) ? ins : outs).push(pts.slice())
    return { ins, outs }
  }
  for (let i = 0; i < pts.length - 1; i++) {
    const a = pts[i]
    const b = pts[i + 1]
    const ts = [0, ...crossings(a, b, poly), 1]
    for (let k = 0; k < ts.length - 1; k++) {
      if (ts[k + 1] - ts[k] < 1e-9) continue
      const m = lerpPt(a, b, (ts[k] + ts[k + 1]) / 2)
      const isIn = pip(poly, m[0], m[1])
      if (cur === null || curIn !== isIn) {
        flush()
        cur = [lerpPt(a, b, ts[k])]
        curIn = isIn
      }
      ;(cur as Pt[]).push(lerpPt(a, b, ts[k + 1]))
    }
  }
  flush()
  return { ins, outs }
}

export const xfXY = (x: number, y: number, f: Xf): [number, number] => [f.cx + f.dx + f.s * (x - f.cx), f.cy + f.dy + f.s * (y - f.cy)]
export const xfPoly = (poly: Poly, f: Xf): Poly => poly.map((p) => xfXY(p[0], p[1], f))
export const isIdentity = (f: Xf) => f.dx === 0 && f.dy === 0 && f.s === 1

export function xfItem(it: Item, f: Xf): Item {
  if (it.tool === 'image') {
    const [x, y, w, h] = it.rect
    const [nx, ny] = xfXY(x, y, f)
    return { ...it, rect: [nx, ny, w * f.s, h * f.s] }
  }
  return { ...it, pts: it.pts.map((q) => [...xfXY(q[0], q[1], f), q[2], q[3]] as Pt), size: it.size * f.s }
}

const anchor = (it: ImageItem): [number, number] => [it.rect[0] + it.rect[2] / 2, it.rect[1] + it.rect[3] / 2]

// ── erased ink ──────────────────────────────────────────────────────────────
// The eraser works on pixels, so a stroke that has been erased over is still all there in the log. Selecting or
// moving it must not resurrect the erased parts (a moved piece lands on top of the erasers, which then can't touch
// it) — so before a piece is selected or moved, the points the erasers visibly wiped are dropped from it. A point
// counts as erased once the erasers after it that pass within their radius have left under ERASED_BELOW of it
// (each pass removes its opacity in total), and only erasers drawn with that point's layer visible count.
const ERASED_BELOW = 0.2
const ERASER_RADIUS = 1.2 // × size, world units (same as the dab)

interface Eraser {
  it: StrokeItem
  r: number
  box: BBox
}
const erasersAfter = (items: Item[], k: number, layer: keyof Layers): Eraser[] => {
  const out: Eraser[] = []
  for (let i = k + 1; i < items.length; i++) {
    const e = items[i]
    if (e.tool !== 'eraser' || (e.only && !e.only.includes(layer))) continue
    const r = e.size * ERASER_RADIUS
    let x0 = 1e9
    let y0 = 1e9
    let x1 = -1e9
    let y1 = -1e9
    for (const q of e.pts) {
      x0 = Math.min(x0, q[0])
      y0 = Math.min(y0, q[1])
      x1 = Math.max(x1, q[0])
      y1 = Math.max(y1, q[1])
    }
    out.push({ it: e, r, box: [x0 - r, y0 - r, x1 + r, y1 + r] })
  }
  return out
}
function distToPath(x: number, y: number, pts: Pt[]): number {
  if (pts.length === 1) return Math.hypot(x - pts[0][0], y - pts[0][1])
  let best = Infinity
  for (let i = 0; i < pts.length - 1; i++) {
    const [ax, ay] = pts[i]
    const dx = pts[i + 1][0] - ax
    const dy = pts[i + 1][1] - ay
    const l2 = dx * dx + dy * dy
    const t = l2 ? Math.max(0, Math.min(1, ((x - ax) * dx + (y - ay) * dy) / l2)) : 0
    best = Math.min(best, Math.hypot(x - ax - t * dx, y - ay - t * dy))
  }
  return best
}
/** A piece cut into the runs of points that haven't been erased away. */
function visibleRuns(pts: Pt[], all: Eraser[]): Pt[][] {
  // only erasers that come near this piece matter
  let x0 = 1e9
  let y0 = 1e9
  let x1 = -1e9
  let y1 = -1e9
  for (const q of pts) {
    x0 = Math.min(x0, q[0])
    y0 = Math.min(y0, q[1])
    x1 = Math.max(x1, q[0])
    y1 = Math.max(y1, q[1])
  }
  const erasers = all.filter((e) => e.box[0] <= x1 && e.box[2] >= x0 && e.box[1] <= y1 && e.box[3] >= y0)
  if (!erasers.length) return [pts]
  // a straight stroke is just its two end points, so look along it: sample often enough to see a gap the width of the narrowest eraser
  const step = Math.max(0.5, Math.min(...erasers.map((e) => e.r)) * 0.4)
  const dense: Pt[] = [pts[0]]
  for (let i = 1; i < pts.length; i++) {
    const a = pts[i - 1]
    const b = pts[i]
    const n = Math.min(400, Math.ceil(Math.hypot(b[0] - a[0], b[1] - a[1]) / step))
    for (let k = 1; k <= n; k++) {
      const t = k / n
      dense.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, a[3] + (b[3] - a[3]) * t])
    }
  }
  pts = dense
  const runs: Pt[][] = []
  let cur: Pt[] = []
  for (const q of pts) {
    let left = 1
    for (const e of erasers) {
      if (q[0] < e.box[0] || q[0] > e.box[2] || q[1] < e.box[1] || q[1] > e.box[3]) continue
      if (distToPath(q[0], q[1], e.it.pts) <= e.r) left *= 1 - (e.it.op ?? 1)
    }
    if (left < ERASED_BELOW) {
      if (cur.length) runs.push(cur)
      cur = []
    } else cur.push(q)
  }
  if (cur.length) runs.push(cur)
  return pts.length > 1 ? runs.filter((r) => r.length > 1) : runs // a lone survivor of a cut is a speck, not a stroke
}

export function fold(evs: Ev[], extra: Ev[] = []): Item[] {
  let items: Item[] = []
  let n = 0
  for (const ev of evs.concat(extra)) {
    if (ev.active === false) continue
    n++
    if (ev.type === 'stroke') {
      items.push({ id: ev.id ?? `s${n}`, tool: ev.tool, seed: ev.seed, size: ev.size, op: ev.op, only: ev.only, pts: ev.pts })
      continue
    }
    if (ev.type === 'image') {
      items.push({ id: ev.id ?? `s${n}`, tool: 'image', hash: ev.hash, rect: ev.rect })
      continue
    }
    const evId = ev.id ?? `e${n}`
    const scope = ev.ids ? new Set(ev.ids) : null
    const keep: Item[] = []
    const moved: Item[] = []
    let pieceNo = 0
    items.forEach((it, k) => {
      if (it.tool === 'eraser') {
        keep.push(it) // erasers are subtractive history, not selectable ink
        return
      }
      if ((ev.only && !ev.only.includes(layerOf(it))) || (scope && !scope.has(it.id))) {
        keep.push(it) // edit was made with that layer hidden / the item isn't part of the selection
        return
      }
      if (it.tool === 'image') {
        const [ax, ay] = anchor(it)
        if (!pip(ev.poly, ax, ay)) {
          keep.push(it)
          return
        }
        if (ev.type === 'move') {
          if (ev.copy) keep.push(it)
          moved.push({ ...(xfItem(it, ev.xf) as ImageItem), id: `${it.id}~${evId}i${pieceNo++}` })
        }
        return
      }
      const { ins, outs } = splitByPoly(it.pts, ev.poly)
      if (!ins.length) {
        keep.push(it)
        return
      }
      const gone = erasersAfter(items, k, layerOf(it))
      const inside = ins.flatMap((p) => visibleRuns(p, gone)) // what's actually there to move / delete
      if (ev.type === 'move') {
        if (ev.copy) keep.push(it)
        else outs.forEach((p, i) => keep.push({ ...it, id: `${it.id}~${evId}o${i}`, pts: p }))
        for (const p of inside) moved.push({ ...(xfItem({ ...it, pts: p }, ev.xf) as StrokeItem), id: `${it.id}~${evId}i${pieceNo}`, seed: (it.seed + 977 * (++pieceNo)) >>> 0 })
      } else outs.forEach((p, i) => keep.push({ ...it, id: `${it.id}~${evId}o${i}`, pts: p }))
    })
    items = keep.concat(moved) // moved pieces land on top (after any earlier erasers)
  }
  return items
}

export type BBox = [number, number, number, number]
export function bboxOf(items: Item[]): BBox {
  let x0 = 1e9
  let y0 = 1e9
  let x1 = -1e9
  let y1 = -1e9
  for (const it of items) {
    if (it.tool === 'image') {
      const [x, y, w, h] = it.rect
      x0 = Math.min(x0, x)
      y0 = Math.min(y0, y)
      x1 = Math.max(x1, x + w)
      y1 = Math.max(y1, y + h)
    } else {
      const m = it.size * 0.6
      for (const q of it.pts) {
        x0 = Math.min(x0, q[0] - m)
        y0 = Math.min(y0, q[1] - m)
        x1 = Math.max(x1, q[0] + m)
        y1 = Math.max(y1, q[1] + m)
      }
    }
  }
  return [x0, y0, x1, y1]
}

/** What a polygon selects: the visible ink inside it — strokes cut at the boundary and minus anything erased, images by
 *  centre; hidden layers can't be selected. `ids` are the items it came from, which is what the move / delete op is
 *  then scoped to, so it never picks up other ink that happens to sit there later. */
export function piecesIn(items: Item[], poly: Poly, layers: Layers): { pieces: Item[]; ids: string[] } {
  const pieces: Item[] = []
  const ids: string[] = []
  items.forEach((it, k) => {
    if (it.tool === 'eraser' || !layers[layerOf(it)]) return
    if (it.tool === 'image') {
      const [ax, ay] = anchor(it)
      if (pip(poly, ax, ay)) {
        pieces.push(it)
        ids.push(it.id)
      }
      return
    }
    const gone = erasersAfter(items, k, layerOf(it))
    let any = false
    for (const p of splitByPoly(it.pts, poly).ins)
      for (const run of visibleRuns(p, gone)) {
        pieces.push({ ...it, pts: run })
        any = true
      }
    if (any) ids.push(it.id)
  })
  return { pieces, ids }
}

/** 2D Ramer-Douglas-Peucker for lasso paths. */
export function simplifyPoly(pts: Poly, tol: number): Poly {
  const n = pts.length
  if (n < 3) return pts.slice()
  const keep = new Uint8Array(n)
  keep[0] = keep[n - 1] = 1
  const stack: [number, number][] = [[0, n - 1]]
  while (stack.length) {
    const [a, b] = stack.pop()!
    const A = pts[a]
    const B = pts[b]
    const dx = B[0] - A[0]
    const dy = B[1] - A[1]
    const l2 = dx * dx + dy * dy
    let md = 0
    let mi = -1
    for (let i = a + 1; i < b; i++) {
      const t = l2 ? Math.max(0, Math.min(1, ((pts[i][0] - A[0]) * dx + (pts[i][1] - A[1]) * dy) / l2)) : 0
      const d = Math.hypot(pts[i][0] - A[0] - t * dx, pts[i][1] - A[1] - t * dy)
      if (d > md) {
        md = d
        mi = i
      }
    }
    if (md > tol && mi > 0) {
      keep[mi] = 1
      stack.push([a, mi], [mi, b])
    }
  }
  return pts.filter((_, i) => keep[i])
}
