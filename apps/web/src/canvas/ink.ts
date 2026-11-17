/** The ink engine: anisotropic-Gaussian "splat" pencil and pen, hard-dab eraser.
 *  Ported from the ink-spike prototype (github.com/clawjungle/ink-spike) with Josh's tuned
 *  defaults baked in (paper tooth 0, tilt shading 0) — only size / opacity remain as knobs.
 *
 *  Strokes are stored as a few simplified points (x, y, pressure, tilt) + a seed; every client
 *  regenerates the pixels. The buffers hold *coverage*, not colour:
 *    pencil — additive deposit depth `covPencil`; inkOf() turns depth into ink against the paper tile.
 *    pen    — Gaussian splats sum into a depth field `covPen`; the line is where depth > 0.5 (metaball
 *             style), so wobble merges. `covPenO` is per-pixel opacity, stacking across strokes but
 *             not within one.
 *    eraser — multiplies all three, so a partial erase both thins and fades. */
import { getPaper, mulberry32, PAPER_N } from './paper'

export type Tool = 'pencil' | 'pen' | 'eraser'
export type Pt = [number, number, number, number] // x, y (world), pressure 0-1, tilt 0-1
export interface StrokeEv {
  type: 'stroke'
  tool: Tool
  seed: number
  size: number
  op?: number // 0-1, omitted = 1
  pts: Pt[]
  active?: boolean
}
export interface ImageEv {
  type: 'image'
  hash: string
  rect: [number, number, number, number] // x, y, w, h in world units
  active?: boolean
}
export type Poly = [number, number][]
/** Move/scale about (cx, cy): p' = c + d + s·(p − c). No rotation. */
export interface Xf {
  dx: number
  dy: number
  s: number
  cx: number
  cy: number
}
/** Everything inside `poly` is cut out at the boundary and transformed by `xf` (originals stay if `copy`).
 *  `only` = the layers the edit was made with visible; other layers are left alone. */
export interface MoveEv {
  type: 'move'
  poly: Poly
  xf: Xf
  copy?: boolean
  only?: string[]
  active?: boolean
}
export interface DeleteEv {
  type: 'delete'
  poly: Poly
  only?: string[]
  active?: boolean
}
export type Ev = StrokeEv | ImageEv | MoveEv | DeleteEv
/** What a month canvas looks like after folding its events: what actually gets drawn, in order. */
export interface StrokeItem {
  tool: Tool
  seed: number
  size: number
  op?: number
  pts: Pt[]
}
export interface ImageItem {
  tool: 'image'
  hash: string
  rect: [number, number, number, number]
}
export type Item = StrokeItem | ImageItem
export interface View {
  x: number
  y: number
  z: number
}
export interface Layers {
  pencil: boolean
  pen: boolean
  image: boolean
}

export const INK: [number, number, number] = [255, 82, 0] // the brand orange
const FLOW = 0.27 // pencil flow (tuned)

const clamp = (v: number, a: number, b: number) => (v < a ? a : v > b ? b : v)

const GLUT = (() => {
  const n = 1024
  const t = new Float32Array(n + 1)
  const e = Math.exp(-4.5)
  for (let i = 0; i <= n; i++) t[i] = (Math.exp((-i / n) * 4.5) - e) / (1 - e)
  return t
})()

const penOf = (d: number) => {
  if (d <= 0.42) return 0
  if (d >= 0.58) return 1
  const t = (d - 0.42) / 0.16
  return t * t * (3 - 2 * t)
}
/** Deposit depth + paper height -> ink alpha. Tooth strength is 0, so only the "pits" term uses the paper. */
const inkOf = (cp: number, tooth: number) => {
  if (cp <= 0) return 0
  return ((0.9 * cp) / (cp + 0.4)) * (cp < 0.15 ? cp * 6.67 : 1) * (1 - 0.6 * tooth * tooth)
}

const cr = (a: number, b: number, c: number, d: number, t: number) =>
  0.5 * (2 * b + (-a + c) * t + (2 * a - 5 * b + 4 * c - d) * t * t + (-a + 3 * b - 3 * c + d) * t * t * t)

export class InkBuffers {
  W = 0
  H = 0
  dpr = 1
  bv: View = { x: 0, y: 0, z: 1 } // the view the buffers were stamped at
  covPencil = new Float32Array(0)
  covPen = new Float32Array(0)
  covPenO = new Float32Array(0)
  penPrev = new Float32Array(0)
  penSid = new Uint32Array(0)
  seq = 0
  dirty: [number, number, number, number] | null = null

  resize(W: number, H: number, dpr: number) {
    this.W = W
    this.H = H
    this.dpr = dpr
    const n = W * H
    this.covPencil = new Float32Array(n)
    this.covPen = new Float32Array(n)
    this.covPenO = new Float32Array(n)
    this.penPrev = new Float32Array(n)
    this.penSid = new Uint32Array(n)
  }

  clear(view: View) {
    this.covPencil.fill(0)
    this.covPen.fill(0)
    this.covPenO.fill(0)
    this.penSid.fill(0)
    this.dirty = null
    this.bv = { ...view }
  }

  markDirty(x0: number, y0: number, x1: number, y1: number) {
    const d = this.dirty
    if (!d) this.dirty = [x0, y0, x1, y1]
    else {
      d[0] = Math.min(d[0], x0)
      d[1] = Math.min(d[1], y0)
      d[2] = Math.max(d[2], x1)
      d[3] = Math.max(d[3], y1)
    }
  }

  /** Anisotropic Gaussian: sigma s1 along ang, s2 across (device px). Σ = R diag(s1²,s2²) Rᵀ, cut at 3σ. */
  splatG(cx: number, cy: number, s1: number, s2: number, ang: number, a: number, buf: Float32Array, op = 0, sid = 0) {
    const { W, H, covPenO, penSid, penPrev } = this
    const c = Math.cos(ang)
    const s = Math.sin(ang)
    const v1 = s1 * s1
    const v2 = s2 * s2
    const hx = 3 * Math.sqrt(v1 * c * c + v2 * s * s)
    const hy = 3 * Math.sqrt(v1 * s * s + v2 * c * c)
    const x0 = Math.max(0, Math.floor(cx - hx - 1))
    const x1 = Math.min(W - 1, Math.ceil(cx + hx + 1))
    const y0 = Math.max(0, Math.floor(cy - hy - 1))
    const y1 = Math.min(H - 1, Math.ceil(cy + hy + 1))
    if (x1 < x0 || y1 < y0) return
    const ia = (c * c) / v1 + (s * s) / v2
    const ic = (s * s) / v1 + (c * c) / v2
    const ib2 = 2 * c * s * (1 / v1 - 1 / v2)
    const K = 1024 / 9
    for (let y = y0; y <= y1; y++) {
      const dy = y + 0.5 - cy
      const row = y * W
      const bx = ib2 * dy
      const cy2 = ic * dy * dy
      for (let x = x0; x <= x1; x++) {
        const dx = x + 0.5 - cx
        const q = ia * dx * dx + bx * dx + cy2
        if (q >= 9) continue
        const w = GLUT[(q * K) | 0]
        const i = row + x
        buf[i] += a * w
        if (op && w > 0.3) {
          // opacity stacks across strokes, not within one: each pixel remembers what was under this stroke
          if (penSid[i] !== sid) {
            penSid[i] = sid
            penPrev[i] = covPenO[i]
          }
          const t = 1 - (1 - penPrev[i]) * (1 - op)
          if (covPenO[i] < t) covPenO[i] = t
        }
      }
    }
    this.markDirty(x0, y0, x1 + 1, y1 + 1)
  }

  /** Hard-edged eraser dab: thins (depth) and fades (opacity) at once. */
  erase(cx: number, cy: number, r: number, a: number) {
    const { W, H, covPencil, covPen, covPenO } = this
    const x0 = Math.max(0, Math.floor(cx - r - 1))
    const x1 = Math.min(W - 1, Math.ceil(cx + r + 1))
    const y0 = Math.max(0, Math.floor(cy - r - 1))
    const y1 = Math.min(H - 1, Math.ceil(cy + r + 1))
    if (x1 < x0 || y1 < y0) return
    const inner = Math.max(0, r - 0.5)
    const inner2 = inner * inner
    const outer2 = (r + 0.5) * (r + 0.5)
    for (let y = y0; y <= y1; y++) {
      const dy = y + 0.5 - cy
      const dy2 = dy * dy
      const row = y * W
      for (let x = x0; x <= x1; x++) {
        const dx = x + 0.5 - cx
        const d2 = dx * dx + dy2
        if (d2 >= outer2) continue
        const w = d2 <= inner2 ? 1 : clamp(r - Math.sqrt(d2) + 0.5, 0, 1)
        if (w <= 0) continue
        const k = 1 - a * w
        const i = row + x
        covPencil[i] *= k
        covPen[i] *= k
        covPenO[i] *= k
      }
    }
    this.markDirty(x0, y0, x1 + 1, y1 + 1)
  }

  /** Stamp a whole stored stroke. */
  stampStroke(st: StrokeItem) {
    const s = new Stamper(this, st)
    for (const p of st.pts) s.push(p)
    s.end()
  }

  /** Paint coverage as brand-coloured RGBA (alpha = ink) into `ctx` for a device-px region. */
  paint(ctx: CanvasRenderingContext2D, layers: Layers, x0: number, y0: number, x1: number, y1: number) {
    const { W, H, bv, dpr } = this
    x0 = clamp(x0 | 0, 0, W)
    y0 = clamp(y0 | 0, 0, H)
    x1 = clamp(Math.ceil(x1), 0, W)
    y1 = clamp(Math.ceil(y1), 0, H)
    const w = x1 - x0
    const h = y1 - y0
    if (w <= 0 || h <= 0) return
    const img = ctx.createImageData(w, h)
    const d = img.data
    const paper = getPaper()
    const S = bv.z * dpr
    const zoomedIn = S > dpr * 1.25
    const gs = dpr
    const g = (a: number, b: number) => paper[(b & 255) * PAPER_N + (a & 255)]
    for (let yy = 0; yy < h; yy++) {
      const Y = y0 + yy
      for (let xx = 0; xx < w; xx++) {
        const X = x0 + xx
        const i = Y * W + X
        let a = layers.pen ? penOf(this.covPen[i]) * this.covPenO[i] : 0
        const cp = layers.pencil ? this.covPencil[i] : 0
        if (cp > 0.002) {
          const fx = (bv.x + X / S) * gs
          const fy = (bv.y + Y / S) * gs
          let tooth: number
          if (zoomedIn) {
            const px = fx - 0.5
            const py = fy - 0.5
            const ix = Math.floor(px)
            const iy = Math.floor(py)
            const tx = px - ix
            const ty = py - iy
            tooth =
              (g(ix, iy) * (1 - tx) + g(ix + 1, iy) * tx) * (1 - ty) +
              (g(ix, iy + 1) * (1 - tx) + g(ix + 1, iy + 1) * tx) * ty
            tooth = clamp((tooth - 0.5) * 1.6 + 0.5, 0, 1)
          } else tooth = g(Math.floor(fx), Math.floor(fy))
          a = 1 - (1 - a) * (1 - inkOf(cp, tooth))
        }
        if (a <= 0) continue
        const o = (yy * w + xx) * 4
        d[o] = INK[0]
        d[o + 1] = INK[1]
        d[o + 2] = INK[2]
        d[o + 3] = a * 255
      }
    }
    ctx.putImageData(img, x0, y0)
  }

  flush(ctx: CanvasRenderingContext2D, layers: Layers) {
    const d = this.dirty
    if (!d) return
    this.dirty = null
    ctx.clearRect(d[0], d[1], d[2] - d[0], d[3] - d[1])
    this.paint(ctx, layers, d[0], d[1], d[2], d[3])
  }
}

/** Turns a point stream into dabs spaced along a Catmull-Rom curve. Used both live and for replay. */
export class Stamper {
  a: Pt[] = []
  si = 0
  rng: () => number
  next = 0
  started = false
  tang = 0
  id: number
  constructor(
    private b: InkBuffers,
    private st: { tool: Tool; seed: number; size: number; op?: number },
  ) {
    this.rng = mulberry32(st.seed)
    this.id = ++b.seq
  }
  push(pt: Pt) {
    if (!this.a.length) this.a.push(pt)
    this.a.push(pt)
    while (this.si + 3 < this.a.length) this.seg(this.si++)
  }
  end() {
    if (!this.a.length) return
    const last = this.a[this.a.length - 1]
    this.a.push(last)
    while (this.si + 3 < this.a.length) this.seg(this.si++)
    if (!this.started) this.stamp(last)
    if (this.st.tool === 'pen') this.stampPen(last, true)
  }
  private S() {
    return this.b.bv.z * this.b.dpr
  }
  private stamp(q: Pt) {
    const { tool } = this.st
    if (tool === 'pencil') return this.stampPencil(q)
    if (tool === 'pen') {
      if (!this.started) this.stampPen(q, true)
      return this.stampPen(q, false)
    }
    return this.stampEraser(q)
  }
  private stampPencil(q: Pt) {
    const S = this.S()
    const st = this.st
    const p = q[2]
    const s = (S * st.size * 0.5 * (0.55 + 0.45 * p)) / 2.3
    const refSp = 0.32 * s
    const sp = Math.max(0.35, refSp)
    const flow = FLOW * (0.1 + 0.9 * Math.pow(p, 1.5))
    const u = this.rng()
    const a = (1 - Math.pow(1 - flow, sp / refSp)) * (0.7 + 0.6 * u) * (st.op ?? 1)
    this.b.splatG((q[0] - this.b.bv.x) * S, (q[1] - this.b.bv.y) * S, s, s, 0, a, this.b.covPencil)
    this.next = sp
    this.started = true
  }
  /** Pen dab: Gaussian elongated along travel (smooths wobble), amplitude normalised so a long line peaks at depth 1
   *  (edge = 0.5 at radius r). Caps are isotropic half-amplitude splats at both ends. */
  private stampPen(q: Pt, cap: boolean) {
    const S = this.S()
    const { bv } = this.b
    const r = S * this.st.size * 0.5 * (0.4 + 0.6 * Math.sqrt(q[2]))
    const s2 = r / 1.177
    const s1 = s2 * 1.6
    const sp = Math.max(0.35, 0.25 * s1)
    const op = this.st.op ?? 1
    if (cap) this.b.splatG((q[0] - bv.x) * S, (q[1] - bv.y) * S, s2, s2, 0, 0.5, this.b.covPen, op, this.id)
    else {
      this.b.splatG((q[0] - bv.x) * S, (q[1] - bv.y) * S, s1, s2, this.tang, sp / (2.507 * s1), this.b.covPen, op, this.id)
      this.next = sp
      this.started = true
    }
  }
  private stampEraser(q: Pt) {
    const S = this.S()
    const { bv } = this.b
    const r = S * this.st.size * 1.2
    const refSp = 0.12 * r
    const sp = Math.max(0.35, refSp)
    const fl = this.st.op ?? 1
    // one pass over a point (2r/sp dabs) removes `op` in total
    const a = fl >= 1 ? 1 : 1 - Math.pow(1 - fl, sp / (2 * r))
    this.b.erase((q[0] - bv.x) * S, (q[1] - bv.y) * S, r, a)
    this.next = sp
    this.started = true
  }
  private seg(i: number) {
    const [P0, P1, P2, P3] = [this.a[i], this.a[i + 1], this.a[i + 2], this.a[i + 3]]
    const S = this.S()
    if (!this.started) {
      this.tang = Math.atan2(P2[1] - P1[1], P2[0] - P1[0])
      this.stamp(P1)
    }
    const L = Math.hypot(P2[0] - P1[0], P2[1] - P1[1]) * S
    const K = Math.max(1, Math.ceil(L / 1.5))
    let prev: Pt = P1
    for (let k = 1; k <= K; k++) {
      const t = k / K
      const cur: Pt = [
        cr(P0[0], P1[0], P2[0], P3[0], t),
        cr(P0[1], P1[1], P2[1], P3[1], t),
        P1[2] + (P2[2] - P1[2]) * t,
        P1[3] + (P2[3] - P1[3]) * t,
      ]
      const d = Math.hypot(cur[0] - prev[0], cur[1] - prev[1]) * S
      if (d > 1e-6) {
        if (d > 0.4 || !this.started) this.tang = Math.atan2(cur[1] - prev[1], cur[0] - prev[0])
        let pos = 0
        while (this.next <= d - pos) {
          pos += this.next
          const f = pos / d
          this.stamp([
            prev[0] + (cur[0] - prev[0]) * f,
            prev[1] + (cur[1] - prev[1]) * f,
            prev[2] + (cur[2] - prev[2]) * f,
            prev[3] + (cur[3] - prev[3]) * f,
          ])
        }
        this.next -= d - pos
      }
      prev = cur
    }
  }
}

/** 4D Ramer-Douglas-Peucker: pressure/tilt are extra dimensions, so a ramp keeps its endpoints. */
export function rdp(pts: Pt[], tol: number, pw = 3): Pt[] {
  const n = pts.length
  if (n < 3 || tol <= 0) return pts.slice()
  const keep = new Uint8Array(n)
  keep[0] = keep[n - 1] = 1
  const stack: [number, number][] = [[0, n - 1]]
  while (stack.length) {
    const [a, b] = stack.pop()!
    const A = pts[a]
    const B = pts[b]
    let md = 0
    let mi = -1
    const ab = [B[0] - A[0], B[1] - A[1], (B[2] - A[2]) * pw, (B[3] - A[3]) * pw]
    const ab2 = ab.reduce((s, v) => s + v * v, 0)
    for (let i = a + 1; i < b; i++) {
      const P = pts[i]
      const ap = [P[0] - A[0], P[1] - A[1], (P[2] - A[2]) * pw, (P[3] - A[3]) * pw]
      const t = ab2 ? clamp(ap.reduce((s, v, k) => s + v * ab[k], 0) / ab2, 0, 1) : 0
      const d = Math.hypot(ap[0] - t * ab[0], ap[1] - t * ab[1], ap[2] - t * ab[2], ap[3] - t * ab[3])
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

/** A little demo of the current tool at its current size / opacity / zoom, for the options panel: two crossing strokes
 *  (so stacking shows) — or, for the eraser, two pen lines and a pencil line with an erasing pass across them. */
export function renderSample(c: HTMLCanvasElement, tool: Tool, size: number, op: number, zoom: number) {
  const dpr = Math.min(window.devicePixelRatio || 1, 2)
  const cw = c.clientWidth || 240
  const ch = c.clientHeight || 64
  c.width = Math.round(cw * dpr)
  c.height = Math.round(ch * dpr)
  const z = clamp(zoom, 0.25, 6)
  const buf = new InkBuffers()
  buf.resize(c.width, c.height, dpr)
  buf.clear({ x: 0, y: 0, z })
  const line = (x0: number, y0: number, x1: number, y1: number, amp: number, n = 40): Pt[] =>
    Array.from({ length: n + 1 }, (_, i) => {
      const t = i / n
      return [(x0 + (x1 - x0) * t) / z, (y0 + (y1 - y0) * t + Math.sin(t * 6.28 * 1.25) * amp) / z, 0.6, 0] as Pt
    })
  const stamp = (t: Tool, sz: number, o: number | undefined, pts: Pt[]) => buf.stampStroke({ tool: t, seed: 7, size: sz, op: o, pts })
  const o = op < 1 ? op : undefined
  if (tool === 'eraser') {
    for (const y of [16, 32]) stamp('pen', 6, undefined, line(14, y, cw - 14, y, 0))
    stamp('pencil', 8, undefined, line(14, 48, cw - 14, 48, 0))
    stamp('eraser', size, o, line(cw / 2, 4, cw / 2, ch - 4, 0, 20))
  } else {
    stamp(tool, size, o, line(14, ch * 0.5, cw - 14, ch * 0.5, ch * 0.22))
    stamp(tool, size, o, line(cw * 0.35, 6, cw * 0.65, ch - 6, 0, 20))
  }
  const ctx = c.getContext('2d')!
  ctx.clearRect(0, 0, c.width, c.height)
  buf.dirty = [0, 0, c.width, c.height]
  buf.flush(ctx, { pencil: true, pen: true, image: true })
}
