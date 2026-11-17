/** Imperative controller for the infinite month canvas: pointer input, pan/zoom, replay, layers.
 *  React owns the chrome (toolbar, menus, layer toggles); this owns the canvases so a stroke never
 *  triggers a React render. Gesture and input handling are ported from the ink-spike prototype,
 *  including the iPad fix (touchstart/touchmove preventDefault — without it iPadOS swallowed a pencil
 *  touch that landed within ~0.4 s of the previous one). */
import { InkBuffers, Stamper, rdp, type Ev, type ImageEv, type Layers, type Pt, type StrokeEv, type Tool, type View } from './ink'
import { getBlob, ingest, loadMonth, monthKey, saveMonth } from './store'

export interface ToolSetting {
  size: number
  op: number
}
export type Settings = Record<Tool, ToolSetting>
// Josh-tuned defaults (ink-spike, 2026-11-17)
export const DEFAULT_SETTINGS: Settings = {
  pencil: { size: 4.5, op: 0.7 },
  pen: { size: 5, op: 1 },
  eraser: { size: 5, op: 0.4 },
}
const SIMPLIFY: Record<Tool, number> = { pencil: 0, pen: 0.4, eraser: 0.4 }
const DIM = 0.3 // other months show through at this opacity
const clamp = (v: number, a: number, b: number) => (v < a ? a : v > b ? b : v)

interface Floating {
  hash: string
  bmp: ImageBitmap
  rect: [number, number, number, number]
}
interface Live {
  tool: Tool
  seed: number
  size: number
  op?: number
  pts: Pt[]
  stamper: Stamper
  pid: number
  sm?: Pt // smoothed pen position (pen only)
  raw?: Pt
  lastT: number
  lastX: number
  lastY: number
  sp?: number
}

export interface ControllerHooks {
  /** Events changed (undo state, month list). */
  onChange(): void
  /** An image is being placed (true) or was placed / cancelled (false). */
  onFloating(on: boolean): void
  /** Right-click at client coordinates. */
  onContext(x: number, y: number): void
}

export class CanvasController {
  readonly month = monthKey()
  tool: Tool = 'pencil'
  settings: Settings = structuredClone(DEFAULT_SETTINGS)
  layers: Layers = { pencil: true, pen: true, image: true }
  visibleMonths: string[] = []
  touchDraws = false
  view: View = { x: 0, y: 0, z: 1 }

  private host: HTMLElement
  private stage: HTMLDivElement
  private base: HTMLCanvasElement
  private imgs: HTMLCanvasElement
  private ink: HTMLCanvasElement
  private over: HTMLCanvasElement
  private ring: HTMLDivElement
  private ctx: Record<'base' | 'imgs' | 'ink' | 'over', CanvasRenderingContext2D>
  private tmpInk = document.createElement('canvas')
  private tmpMonth = document.createElement('canvas')
  private buf = new InkBuffers()
  private dpr = Math.min(window.devicePixelRatio || 1, 2)
  private events: Ev[]
  private others = new Map<string, Ev[]>()
  private bitmaps = new Map<string, ImageBitmap | 'loading'>()
  private live: Live | null = null
  private floating: Floating | null = null
  private floatDrag: { mode: 'move' | 'scale'; sx: number; sy: number; rect: Floating['rect'] } | null = null
  private ptrs = new Map<number, { x: number; y: number; pan: boolean }>()
  private penSeen = false
  private lastPenT = 0
  private spaceDown = false
  private prevG: { cx: number; cy: number; dist: number; n: number } | null = null
  private rbTimer = 0
  private ro: ResizeObserver
  private off: (() => void)[] = []

  constructor(host: HTMLElement, private hooks: ControllerHooks, saved?: Partial<{ settings: Settings; view: View }>) {
    this.host = host
    if (saved?.settings) this.settings = { ...this.settings, ...saved.settings }
    if (saved?.view) this.view = { ...saved.view }
    this.events = loadMonth(this.month)
    this.stage = document.createElement('div')
    this.stage.className = 'canvas-stage'
    const mk = (cls: string) => {
      const c = document.createElement('canvas')
      c.className = 'canvas-layer ' + cls
      this.stage.appendChild(c)
      return c
    }
    this.base = mk('base')
    this.imgs = mk('imgs')
    this.ink = mk('ink')
    this.over = mk('over')
    this.ring = document.createElement('div')
    this.ring.className = 'canvas-ring'
    host.append(this.stage, this.ring)
    const g = (c: HTMLCanvasElement, willRead = false) => c.getContext('2d', willRead ? { willReadFrequently: true } : undefined)!
    this.ctx = { base: g(this.base), imgs: g(this.imgs), ink: g(this.ink, true), over: g(this.over) }

    const on = <K extends keyof HTMLElementEventMap>(t: K, f: (e: HTMLElementEventMap[K]) => void, o?: AddEventListenerOptions) => {
      host.addEventListener(t, f as EventListener, o)
      this.off.push(() => host.removeEventListener(t, f as EventListener, o))
    }
    const onWin = <K extends keyof WindowEventMap>(t: K, f: (e: WindowEventMap[K]) => void) => {
      window.addEventListener(t, f as EventListener)
      this.off.push(() => window.removeEventListener(t, f as EventListener))
    }
    // iPad: stop WebKit's double-tap / gesture heuristics eating a quick pencil re-landing
    on('touchstart', (e) => e.preventDefault(), { passive: false })
    on('touchmove', (e) => e.preventDefault(), { passive: false })
    on('pointerdown', (e) => this.pointerDown(e))
    on('pointermove', (e) => this.pointerMove(e))
    on('pointerup', (e) => this.pointerUp(e))
    on('pointercancel', (e) => this.pointerUp(e))
    on('lostpointercapture', (e) => this.live?.pid === e.pointerId && this.pointerUp(e))
    on('pointerleave', () => (this.ring.style.display = 'none'))
    on('wheel', (e) => this.wheel(e), { passive: false })
    on('contextmenu', (e) => {
      e.preventDefault()
      this.hooks.onContext(e.clientX, e.clientY)
    })
    on('dragover', (e) => e.preventDefault())
    on('drop', (e) => {
      e.preventDefault()
      const f = [...(e.dataTransfer?.files ?? [])].find((f) => f.type.startsWith('image/'))
      if (f) void this.addImage(f)
    })
    onWin('keydown', (e) => this.key(e, true))
    onWin('keyup', (e) => this.key(e, false))
    onWin('paste', (e) => {
      const f = [...(e.clipboardData?.files ?? [])].find((f) => f.type.startsWith('image/'))
      if (f) void this.addImage(f)
    })
    this.ro = new ResizeObserver(() => this.resize())
    this.ro.observe(host)
    this.resize()
  }

  dispose() {
    window.clearTimeout(this.rbTimer)
    this.ro.disconnect()
    this.off.forEach((f) => f())
    this.stage.remove()
    this.ring.remove()
  }

  // ── public controls ────────────────────────────────────────────────────
  setTool(t: Tool) {
    this.tool = t
    this.host.style.cursor = t === 'eraser' ? 'none' : 'crosshair'
    if (t !== 'eraser') this.ring.style.display = 'none'
  }
  setSetting(t: Tool, s: Partial<ToolSetting>) {
    Object.assign(this.settings[t], s)
  }
  setLayers(l: Layers) {
    this.layers = { ...l }
    this.rebuild()
  }
  setVisibleMonths(ms: string[]) {
    this.visibleMonths = ms.filter((m) => m !== this.month)
    this.rebuild()
  }
  get canUndo() {
    return this.events.some((e) => e.active !== false)
  }
  get canRedo() {
    return this.events.some((e) => e.active === false)
  }
  undo() {
    for (let i = this.events.length - 1; i >= 0; i--)
      if (this.events[i].active !== false) {
        this.events[i].active = false
        this.persist()
        this.rebuild()
        return
      }
  }
  redo() {
    const ev = this.events.find((e) => e.active === false)
    if (ev) {
      ev.active = true
      this.persist()
      this.rebuild()
    }
  }
  /** Pick an image from disk / clipboard and float it for placing. */
  async addImage(file: Blob) {
    if (this.floating) this.placeImage()
    const { hash, bmp, w, h } = await ingest(file)
    this.bitmaps.set(hash, bmp)
    const vw = this.host.clientWidth / this.view.z
    const vh = this.host.clientHeight / this.view.z
    const rw = Math.min(w, vw * 0.4)
    const rh = (rw * h) / w
    this.floating = { hash, bmp, rect: [this.view.x + vw / 2 - rw / 2, this.view.y + vh / 2 - rh / 2, rw, rh] }
    this.hooks.onFloating(true)
    this.drawOverlay()
  }
  placeImage() {
    const f = this.floating
    if (!f) return
    this.floating = null
    this.floatDrag = null
    this.pushEvent({ type: 'image', hash: f.hash, rect: f.rect.map((v) => +v.toFixed(1)) as ImageEv['rect'] })
    this.hooks.onFloating(false)
    this.rebuild()
  }
  cancelImage() {
    if (!this.floating) return
    this.floating = null
    this.floatDrag = null
    this.hooks.onFloating(false)
    this.drawOverlay()
  }
  resetView() {
    this.view = { x: 0, y: 0, z: 1 }
    this.rebuild()
  }

  // ── sizing & rendering ─────────────────────────────────────────────────
  private resize() {
    const cw = this.host.clientWidth
    const ch = this.host.clientHeight
    if (!cw || !ch) return
    const W = Math.round(cw * this.dpr)
    const H = Math.round(ch * this.dpr)
    for (const c of [this.base, this.imgs, this.ink, this.over, this.tmpInk, this.tmpMonth]) {
      c.width = W
      c.height = H
    }
    for (const c of [this.base, this.imgs, this.ink, this.over]) {
      c.style.width = cw + 'px'
      c.style.height = ch + 'px'
    }
    this.buf.resize(W, H, this.dpr)
    this.rebuild()
  }

  private bitmapFor(hash: string): ImageBitmap | null {
    const b = this.bitmaps.get(hash)
    if (b && b !== 'loading') return b
    if (!b) {
      this.bitmaps.set(hash, 'loading')
      void getBlob(hash).then(async (blob) => {
        if (!blob) return this.bitmaps.delete(hash)
        this.bitmaps.set(hash, await createImageBitmap(blob))
        this.schedRebuild(30)
      })
    }
    return null
  }

  private drawImages(ctx: CanvasRenderingContext2D, evs: Ev[]) {
    if (!this.layers.image) return
    const S = this.buf.bv.z * this.dpr
    for (const e of evs) {
      if (e.type !== 'image' || e.active === false) continue
      const bmp = this.bitmapFor(e.hash)
      if (bmp) ctx.drawImage(bmp, (e.rect[0] - this.buf.bv.x) * S, (e.rect[1] - this.buf.bv.y) * S, e.rect[2] * S, e.rect[3] * S)
    }
  }

  private stampAll(evs: Ev[]) {
    for (const e of evs) if (e.type === 'stroke' && e.active !== false) this.buf.stampStroke(e)
  }

  private monthEvents(m: string) {
    let e = this.others.get(m)
    if (!e) this.others.set(m, (e = loadMonth(m)))
    return e
  }

  rebuild() {
    window.clearTimeout(this.rbTimer)
    this.rbTimer = 0
    const { W, H } = this.buf
    if (!W) return
    this.view.z = clamp(this.view.z, 0.1, 20)
    this.stage.style.transform = ''
    const { base, imgs, ink } = this.ctx
    // other months, oldest first, dimmed, underneath
    base.clearRect(0, 0, W, H)
    const tm = this.tmpMonth.getContext('2d')!
    const ti = this.tmpInk.getContext('2d')!
    for (const m of [...this.visibleMonths].sort()) {
      const evs = this.monthEvents(m)
      this.buf.clear(this.view)
      this.stampAll(evs)
      tm.clearRect(0, 0, W, H)
      this.drawImages(tm, evs)
      this.buf.dirty = [0, 0, W, H]
      ti.clearRect(0, 0, W, H)
      this.buf.flush(ti, this.layers)
      tm.drawImage(this.tmpInk, 0, 0)
      base.globalAlpha = DIM
      base.drawImage(this.tmpMonth, 0, 0)
      base.globalAlpha = 1
    }
    // this month: images, then ink (the ink buffers stay live for incremental strokes)
    imgs.clearRect(0, 0, W, H)
    this.drawImages(imgs, this.events)
    this.buf.clear(this.view)
    this.stampAll(this.events)
    this.buf.dirty = [0, 0, W, H]
    this.buf.flush(ink, this.layers)
    this.drawOverlay()
    this.saveView()
  }

  private schedRebuild(ms = 150) {
    window.clearTimeout(this.rbTimer)
    this.rbTimer = window.setTimeout(() => {
      if (this.live || this.floatDrag || [...this.ptrs.values()].some((p) => !p.pan)) return this.schedRebuild(250)
      this.rebuild()
    }, ms)
  }
  private settle() {
    if (this.rbTimer) this.rebuild()
  }

  private saveViewT = 0
  private saveView() {
    window.clearTimeout(this.saveViewT)
    this.saveViewT = window.setTimeout(() => this.hooks.onChange(), 400)
  }

  private drawOverlay() {
    const { W, H } = this.buf
    const o = this.ctx.over
    o.clearRect(0, 0, W, H)
    const f = this.floating
    if (!f) return
    const S = this.view.z * this.dpr
    const [x, y, w, h] = f.rect
    const dx = (x - this.view.x) * S
    const dy = (y - this.view.y) * S
    o.drawImage(f.bmp, dx, dy, w * S, h * S)
    o.strokeStyle = '#ff5200'
    o.lineWidth = 1.5 * this.dpr
    o.setLineDash([6 * this.dpr, 4 * this.dpr])
    o.strokeRect(dx, dy, w * S, h * S)
    o.setLineDash([])
    o.fillStyle = '#ff5200'
    o.beginPath()
    o.arc(dx + w * S, dy + h * S, 8 * this.dpr, 0, 6.3)
    o.fill()
  }

  // ── events ─────────────────────────────────────────────────────────────
  private pushEvent(ev: Ev) {
    while (this.events.length && this.events[this.events.length - 1].active === false) this.events.pop()
    this.events.push({ ...ev, active: true })
    this.persist()
  }
  private persist() {
    saveMonth(this.month, this.events)
    this.hooks.onChange()
  }
  /** Months the user has since left behind need re-reading if another tab drew in them. */
  invalidateOthers() {
    this.others.clear()
  }

  // ── pointer input ──────────────────────────────────────────────────────
  private toWorld(cx: number, cy: number): [number, number] {
    return [this.view.x + cx / this.view.z, this.view.y + cy / this.view.z]
  }
  private local(e: PointerEvent): [number, number] {
    const r = this.host.getBoundingClientRect()
    return [e.clientX - r.left, e.clientY - r.top]
  }

  private moveRing(e: PointerEvent) {
    if (this.tool !== 'eraser' || this.floating) return void (this.ring.style.display = 'none')
    const d = 2 * 1.2 * this.settings.eraser.size * this.view.z
    const r = this.host.getBoundingClientRect()
    this.ring.style.cssText = `display:block;width:${d}px;height:${d}px;transform:translate(${e.clientX - r.left - d / 2}px,${e.clientY - r.top - d / 2}px);background:rgb(255 82 0 / ${0.06 + 0.3 * this.settings.eraser.op})`
  }

  private pointerDown(e: PointerEvent) {
    const now = performance.now()
    if (e.pointerType === 'touch' && this.penSeen && (this.live || now - this.lastPenT < 700)) return e.preventDefault() // palm
    try {
      this.host.setPointerCapture(e.pointerId)
    } catch {
      /* already cancelled */
    }
    if (e.pointerType === 'pen') {
      this.penSeen = true
      this.lastPenT = now
      for (const [id, q] of this.ptrs) if (q.pan) this.ptrs.delete(id)
      this.prevG = null
    }
    const draws = !(e.button === 1 || this.spaceDown) && (e.pointerType !== 'touch' || (this.touchDraws && !this.penSeen))
    const [lx, ly] = this.local(e)
    // lift-and-return: the previous stroke's pointerup hasn't arrived — finish it, never swallow the new one
    if (draws && this.live && (e.pointerType !== 'touch' || this.live.pid === e.pointerId)) {
      this.ptrs.delete(this.live.pid)
      this.endStroke()
    }
    this.ptrs.set(e.pointerId, { x: lx, y: ly, pan: !draws })
    if (draws && !this.live) {
      if (this.floating) this.floatDown(e)
      else this.startStroke(e)
    } else {
      this.prevG = null
      this.applyGesture()
    }
    this.moveRing(e)
    e.preventDefault()
  }

  private pointerMove(e: PointerEvent) {
    this.moveRing(e)
    const p = this.ptrs.get(e.pointerId)
    if (!p) return
    const [lx, ly] = this.local(e)
    p.x = lx
    p.y = ly
    if (e.pointerType === 'pen') this.lastPenT = performance.now()
    if (p.pan) this.applyGesture()
    else if (this.live) this.addPoints(e)
    else if (this.floatDrag) this.floatMove(e)
  }

  private pointerUp(e: PointerEvent) {
    if (e.pointerType === 'pen') this.lastPenT = performance.now()
    const p = this.ptrs.get(e.pointerId)
    if (!p) return
    this.ptrs.delete(e.pointerId)
    if (p.pan) this.prevG = null
    else if (this.live) this.endStroke()
    else if (this.floatDrag) {
      this.floatDrag = null
      this.rebuild()
    }
    if (e.pointerType === 'touch') this.ring.style.display = 'none'
  }

  // ── strokes ────────────────────────────────────────────────────────────
  private startStroke(e: PointerEvent) {
    this.settle()
    const t = this.tool
    const s = this.settings[t]
    const seed = (Math.random() * 4294967296) >>> 0
    const st = { tool: t, seed, size: s.size, op: s.op < 1 ? s.op : undefined }
    this.live = { ...st, pts: [], stamper: new Stamper(this.buf, st), pid: e.pointerId, lastT: 0, lastX: 0, lastY: 0 }
    this.addPoints(e, true)
  }

  private pointFrom(e: PointerEvent, first: boolean): Pt {
    const live = this.live!
    const [lx, ly] = this.local(e)
    const [wx, wy] = this.toWorld(lx, ly)
    let p = e.pressure
    let tilt = 0
    if (e.pointerType === 'mouse' || (e.pointerType === 'touch' && !p)) {
      // no pressure: fake it from speed
      const dt = Math.max(1, e.timeStamp - live.lastT)
      const v = first ? 0 : Math.hypot(wx - live.lastX, wy - live.lastY) / dt
      p = first ? 0.5 : clamp(0.95 - v * 0.28, 0.25, 0.95)
      live.sp = live.sp == null ? p : live.sp * 0.8 + p * 0.2
      p = live.sp
    }
    if (e.pointerType === 'pen') {
      if (e.altitudeAngle !== undefined && e.altitudeAngle !== Math.PI / 2) tilt = 1 - e.altitudeAngle / (Math.PI / 2)
      else tilt = clamp(Math.hypot(e.tiltX || 0, e.tiltY || 0) / 70, 0, 1)
    }
    live.lastT = e.timeStamp
    live.lastX = wx
    live.lastY = wy
    return [wx, wy, clamp(p, 0.02, 1), tilt]
  }

  /** Pen only: exponential low-pass on position + pressure (stabiliser). Stored points are the smoothed ones. */
  private smoothPen(p: Pt): Pt {
    const live = this.live!
    live.raw = p
    if (live.tool !== 'pen') return p
    const m = live.sm
    if (!m) {
      live.sm = [...p] as Pt
      return p
    }
    m[0] += (p[0] - m[0]) * 0.4
    m[1] += (p[1] - m[1]) * 0.4
    m[2] += (p[2] - m[2]) * 0.25
    m[3] = p[3]
    return [...m] as Pt
  }

  private addPoints(e: PointerEvent, first = false) {
    const live = this.live!
    const evs = !first && e.getCoalescedEvents?.().length ? e.getCoalescedEvents() : [e]
    for (const ev of evs) {
      const p = this.smoothPen(this.pointFrom(ev, first && ev === e))
      live.pts.push(p)
      live.stamper.push(p)
    }
    this.buf.flush(this.ctx.ink, this.layers)
  }

  private endStroke() {
    const live = this.live
    if (!live) return
    this.live = null
    try {
      if (live.tool === 'pen' && live.sm && live.raw) {
        for (let i = 0; i < 6; i++) {
          // catch up to the pen on lift
          const m = live.sm
          const r = live.raw
          m[0] += (r[0] - m[0]) * 0.5
          m[1] += (r[1] - m[1]) * 0.5
          const q = [...m] as Pt
          live.pts.push(q)
          live.stamper.push(q)
        }
      }
      live.stamper.end()
      this.buf.flush(this.ctx.ink, this.layers)
      const kept = rdp(live.pts, SIMPLIFY[live.tool]).map((q) => [+q[0].toFixed(1), +q[1].toFixed(1), +q[2].toFixed(2), +q[3].toFixed(2)] as Pt)
      const ev: StrokeEv = { type: 'stroke', tool: live.tool, seed: live.seed, size: live.size, pts: kept }
      if (live.op != null) ev.op = live.op
      this.pushEvent(ev)
    } catch (err) {
      console.error(err)
    }
  }

  // ── floating image ─────────────────────────────────────────────────────
  private floatDown(e: PointerEvent) {
    const f = this.floating!
    const [lx, ly] = this.local(e)
    const [wx, wy] = this.toWorld(lx, ly)
    const [x, y, w, h] = f.rect
    const handle = 16 / this.view.z
    if (Math.hypot(wx - (x + w), wy - (y + h)) <= handle) this.floatDrag = { mode: 'scale', sx: wx, sy: wy, rect: [...f.rect] }
    else if (wx >= x && wx <= x + w && wy >= y && wy <= y + h) this.floatDrag = { mode: 'move', sx: wx, sy: wy, rect: [...f.rect] }
  }
  private floatMove(e: PointerEvent) {
    const f = this.floating
    const d = this.floatDrag
    if (!f || !d) return
    const [lx, ly] = this.local(e)
    const [wx, wy] = this.toWorld(lx, ly)
    const [x, y, w, h] = d.rect
    if (d.mode === 'move') f.rect = [x + wx - d.sx, y + wy - d.sy, w, h]
    else {
      const k = Math.max(0.05, (w + (wx - d.sx)) / w)
      f.rect = [x, y, w * k, h * k]
    }
    this.drawOverlay()
  }

  // ── view: pan / zoom ───────────────────────────────────────────────────
  private gestureState() {
    const pans = [...this.ptrs.values()].filter((p) => p.pan)
    if (!pans.length) return null
    const cx = pans.reduce((s, p) => s + p.x, 0) / pans.length
    const cy = pans.reduce((s, p) => s + p.y, 0) / pans.length
    const dist = pans.length > 1 ? Math.hypot(pans[0].x - pans[1].x, pans[0].y - pans[1].y) : 0
    return { cx, cy, dist, n: pans.length }
  }
  private applyGesture() {
    const g = this.gestureState()
    if (!g) return void (this.prevG = null)
    const pg = this.prevG
    if (pg && pg.n === g.n) {
      const nz = g.n > 1 && pg.dist > 0 ? clamp((this.view.z * g.dist) / pg.dist, 0.1, 20) : this.view.z
      const wx = this.view.x + pg.cx / this.view.z
      const wy = this.view.y + pg.cy / this.view.z
      this.view.z = nz
      this.view.x = wx - g.cx / nz
      this.view.y = wy - g.cy / nz
      this.gestureTransform()
    }
    this.prevG = g
  }
  private wheel(e: WheelEvent) {
    e.preventDefault()
    const r = this.host.getBoundingClientRect()
    const cx = e.clientX - r.left
    const cy = e.clientY - r.top
    if (e.ctrlKey) {
      const nz = clamp(this.view.z * Math.exp(-e.deltaY * 0.01), 0.1, 20)
      const wx = this.view.x + cx / this.view.z
      const wy = this.view.y + cy / this.view.z
      this.view.z = nz
      this.view.x = wx - cx / nz
      this.view.y = wy - cy / nz
    } else {
      this.view.x += e.deltaX / this.view.z
      this.view.y += e.deltaY / this.view.z
    }
    this.gestureTransform()
  }
  /** During a gesture the existing pixels just move (CSS); the sharp replay follows once it settles. */
  private gestureTransform() {
    const bv = this.buf.bv
    const s = this.view.z / bv.z
    const tx = this.view.z * (bv.x - this.view.x)
    const ty = this.view.z * (bv.y - this.view.y)
    this.stage.style.transform = `translate(${tx}px,${ty}px) scale(${s})`
    this.schedRebuild(180)
  }

  private key(e: KeyboardEvent, down: boolean) {
    if (/^(INPUT|TEXTAREA|SELECT)$/.test((e.target as HTMLElement)?.tagName)) return
    if (e.code === 'Space') this.spaceDown = down
    if (!down) return
    const mod = e.metaKey || e.ctrlKey
    if (mod && e.key === 'z') {
      e.preventDefault()
      e.shiftKey ? this.redo() : this.undo()
    } else if (this.floating && e.key === 'Enter') this.placeImage()
    else if (this.floating && e.key === 'Escape') this.cancelImage()
  }
}
