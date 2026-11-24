/** Imperative controller for the infinite month canvas: pointer input, pan/zoom, replay, layers.
 *  React owns the chrome (toolbar, menus, layer toggles); this owns the canvases so a stroke never
 *  triggers a React render. Gesture and input handling are ported from the ink-spike prototype,
 *  including the iPad fix (touchstart/touchmove preventDefault — without it iPadOS swallowed a pencil
 *  touch that landed within ~0.4 s of the previous one). */
import { InkBuffers, Stamper, rdp, type Ev, type ImageEv, type Item, type Layers, type Poly, type Pt, type StrokeEv, type Tool, type View, type Xf } from './ink'
import { bboxOf, fold, isIdentity, layerOf, onlyList, piecesIn, simplifyPoly, xfItem, xfPoly, xfXY, type BBox } from './select'
import type { MonthLog } from './oplog'
import { getBlob, ingest, monthKey } from './store'
import type { CanvasData } from './sync'

export interface ToolSetting {
  size: number
  op: number
}
export type Settings = Record<Tool, ToolSetting>
/** The toolbar's tools: three that draw, two that select. */
/** `timeline` = a lasso whose selection reaches every earlier period that is showing: move, duplicate or erase it across time. */
export type UiTool = Tool | 'lasso' | 'rect' | 'timeline'
/** A 24 px cursor drawn from the tool's icon: a white halo under the orange line so it reads on light and dark, with the
 *  hot spot at the working tip. */
const cursorOf = (path: string, hx: number, hy: number, dash = '', fill = 'none') => {
  const p = `<path d='${path}' stroke-linecap='round' stroke-linejoin='round' ${dash}`
  const svg = `<svg xmlns='http://www.w3.org/2000/svg' width='24' height='24' viewBox='0 0 24 24'>${p} fill='${fill === 'none' ? 'none' : 'white'}' stroke='white' stroke-width='4.2'/>${p} fill='${fill}' stroke='#ff5200' stroke-width='1.8'/></svg>`
  return `url("data:image/svg+xml,${encodeURIComponent(svg)}") ${hx} ${hy}, crosshair`
}
const CURSORS: Record<UiTool, string> = {
  pencil: cursorOf('M4 20l1-4L16 5l3 3L8 19zM14 7l3 3', 4, 20),
  pen: cursorOf('M16 3.5l4.5 4.5L8 20.5 3 21.5l1-5z', 3, 21, '', '#ff5200'),
  eraser: 'none', // the ring is the cursor
  lasso: cursorOf('M6 9c0-3 6-5 10-3s3 7-1 9-8 1-9-2M10 17c0 2 1 3 3 4', 13, 21, "stroke-dasharray='3 2.4'"),
  rect: 'crosshair',
  timeline: cursorOf('M6 9c0-3 6-5 10-3s3 7-1 9-8 1-9-2M10 17c0 2 1 3 3 4M4 4l16 16', 13, 21, "stroke-dasharray='3 2.4'"),
}
/** The part of a selection that lives in one earlier period. */
interface SelPart {
  month: string
  ids: string[]
  pieces: Item[]
}
const isSelect = (t: UiTool) => t === 'lasso' || t === 'rect' || t === 'timeline'
const isDraw = (t: UiTool): t is Tool => !isSelect(t)
// Josh-tuned defaults (ink-spike, 2026-11-17)
export const DEFAULT_SETTINGS: Settings = {
  pencil: { size: 4.5, op: 0.7 },
  pen: { size: 5, op: 1 },
  eraser: { size: 5, op: 0.4 },
}
const SIMPLIFY: Record<Tool, number> = { pencil: 0, pen: 0.4, eraser: 0.4 }
const DIM = 0.3 // earlier periods show through at this opacity (unless dimming is switched off: then at full brightness)
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
  only?: string[]
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
  /** Finger drawing switched itself off (a pen showed up). */
  onTouchDraws(on: boolean): void
  /** The offset marker was added or cleared. */
  onMark(on: boolean): void
  /** The current period was shown again by itself (a tool touched the canvas while it was hidden). */
  onHideCurrent?(hidden: boolean): void
  /** The zoom changed (1 = 100 %). */
  onZoom(z: number): void
  /** The tool changed (by the user, or by a swap from a pen button). */
  onTool(tool: UiTool, prev: UiTool | null): void
  /** Where the selection's top-right corner is (host px), or null when nothing is selected. */
  onSelection(corner: { x: number; y: number } | null): void
  /** A right click that didn't turn into a pan, at client coordinates. */
  onContext(x: number, y: number): void
}

export class CanvasController {
  readonly month = monthKey()
  tool: UiTool = 'pencil'
  /** The tool before this one (what the swap button / key / pencil squeeze goes back to). */
  prevTool: UiTool | null = null
  settings: Settings = structuredClone(DEFAULT_SETTINGS)
  layers: Layers = { pencil: true, pen: true, image: true }
  visibleMonths: string[] = []
  /** One finger draws (phones). Turns itself off the first time a pen is seen, so on an iPad fingers go back to panning. */
  touchDraws = false
  view: View = { x: 0, y: 0, z: 1 }

  private host: HTMLElement
  private stage: HTMLDivElement
  private base: HTMLCanvasElement
  private imgs: HTMLCanvasElement
  private ink: HTMLCanvasElement
  private float: HTMLCanvasElement
  private over: HTMLCanvasElement
  private ring: HTMLDivElement
  private mini: HTMLCanvasElement
  private miniLayer = document.createElement('canvas') // the content, drawn once per change; the viewport rect is drawn over it
  private miniKey = ''
  private miniBounds: { key: string; b: [number, number, number, number] | null } = { key: '', b: null }
  private miniMap = { minX: 0, minY: 0, scale: 1, offX: 0, offY: 0 }
  private miniOn = false
  private miniDrag = false
  private ctx: Record<'base' | 'imgs' | 'ink' | 'float' | 'over', CanvasRenderingContext2D>
  private tmpInk = document.createElement('canvas')
  private tmpMonth = document.createElement('canvas')
  private buf = new InkBuffers()
  private dpr = Math.min(window.devicePixelRatio || 1, 2)
  private log: MonthLog
  private get events(): Ev[] {
    return this.log.events()
  }
  private items: Item[] = []
  /** The selection. `ids`/`pieces` are this period's; `others` are the same shape's ink in each earlier period that is showing (wipe tool only). */
  private sel: { poly: Poly; ids: string[]; pieces: Item[]; others: SelPart[]; bbox: BBox } | null = null
  private lassoPath: Poly | null = null
  private rectStart: [number, number] | null = null
  private lift: { poly: Poly; only?: string[]; ids: string[] } | null = null // the selection is being dragged: draw the canvas without it
  private liftOthers = new Map<string, { poly: Poly; only?: string[]; ids: string[] }>() // ...and the same for each earlier period it reaches
  private drag: { mode: 'move' | 'scale'; sx: number; sy: number; d0: number; cx: number; cy: number } | null = null
  private dragXf: Xf | null = null
  /** Offset mode (finger): the tool's position (world) — a tap sets it, a drag moves it relatively. */
  private mark: [number, number] | null = null
  private off: { qx: number; qy: number; started: boolean } | null = null // active offset-mode drag
  private pend: { qx: number; qy: number; e0: PointerEvent; done: boolean; timer: number } | null = null // finger down, not yet drawing
  private selActive = false // a lasso path or a selection drag is in progress
  private others = new Map<string, { v: number; items: Item[] }>()
  // caches (see rebuild): what's already drawn is kept, and only what changed is redone
  private bboxCache = new Map<string, BBox>() // per item id: its ink bounds (an item's points never change under the same id)
  private baseKey = '' // what the dimmed-months layer currently shows
  private dimLevel = DIM // how bright the earlier periods are drawn: DIM, or 1 when dimming is off
  private hideCurrent = false // this period is hidden so the earlier ones show through (at full brightness)
  private bitmapsLoaded = 0
  private stamped: string[] = [] // the events (id:active) drawn into the ink buffers, in order — lets a peer's new strokes be stamped on top instead of redrawing everything

  private bitmaps = new Map<string, ImageBitmap | 'loading'>()
  private live: Live | null = null
  private floating: Floating | null = null
  private floatDrag: { mode: 'move' | 'scale'; sx: number; sy: number; rect: Floating['rect'] } | null = null
  private ptrs = new Map<number, { x: number; y: number; pan: boolean; touch: boolean }>()
  private rightDown: { id: number; x: number; y: number; moved: boolean } | null = null // a right press that is still a click
  private penSeen = false
  private lastPenT = 0
  private spaceDown = false
  private prevG: { cx: number; cy: number; dist: number; n: number } | null = null
  private rbTimer = 0
  private ro: ResizeObserver
  private cleanups: (() => void)[] = []

  constructor(host: HTMLElement, private hooks: ControllerHooks, private data: CanvasData, saved?: Partial<{ settings: Settings; view: View; touchDraws: boolean }>) {
    this.host = host
    if (saved?.settings) this.settings = { ...this.settings, ...saved.settings }
    if (saved?.view) this.view = { ...saved.view }
    this.touchDraws = saved?.touchDraws ?? false
    this.log = data.log(this.month)
    data.watch([], this.month)
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
    this.float = mk('float')
    this.over = mk('over')
    this.ring = document.createElement('div')
    this.ring.className = 'canvas-ring'
    this.mini = document.createElement('canvas')
    this.mini.className = 'canvas-minimap'
    this.mini.style.display = 'none'
    host.append(this.stage, this.ring, this.mini)
    this.mini.addEventListener('pointerdown', (e) => {
      e.stopPropagation() // the minimap is its own thing, not a drawing surface
      e.preventDefault()
      this.mini.setPointerCapture(e.pointerId)
      this.miniDrag = true
      this.miniGo(e)
    })
    this.mini.addEventListener('pointermove', (e) => this.miniDrag && this.miniGo(e))
    this.mini.addEventListener('pointerup', () => (this.miniDrag = false))
    this.mini.addEventListener('pointercancel', () => (this.miniDrag = false))
    this.mini.addEventListener('touchstart', (e) => e.preventDefault(), { passive: false })
    const g = (c: HTMLCanvasElement, willRead = false) => c.getContext('2d', willRead ? { willReadFrequently: true } : undefined)!
    this.ctx = { base: g(this.base), imgs: g(this.imgs), ink: g(this.ink, true), float: g(this.float), over: g(this.over) }

    const on = <K extends keyof HTMLElementEventMap>(t: K, f: (e: HTMLElementEventMap[K]) => void, o?: AddEventListenerOptions) => {
      host.addEventListener(t, f as EventListener, o)
      this.cleanups.push(() => host.removeEventListener(t, f as EventListener, o))
    }
    const onWin = <K extends keyof WindowEventMap>(t: K, f: (e: WindowEventMap[K]) => void) => {
      window.addEventListener(t, f as EventListener)
      this.cleanups.push(() => window.removeEventListener(t, f as EventListener))
    }
    // iPad: stop WebKit's double-tap / gesture heuristics eating a quick pencil re-landing
    on('touchstart', (e) => e.preventDefault(), { passive: false })
    on('touchmove', (e) => e.preventDefault(), { passive: false })
    on('pointerdown', (e) => this.pointerDown(e))
    on('pointermove', (e) => this.pointerMove(e))
    on('pointerup', (e) => this.pointerUp(e))
    on('pointercancel', (e) => this.pointerUp(e))
    on('lostpointercapture', (e) => this.live?.pid === e.pointerId && this.pointerUp(e))
    on('pointerleave', () => {
      if (!this.mark) this.ring.style.display = 'none' // the mark stays where the tool is
    })
    on('wheel', (e) => this.wheel(e), { passive: false })
    on('contextmenu', (e) => e.preventDefault()) // the right button is ours: drag pans, a plain click opens our menu (below)
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
    this.cleanups.forEach((f) => f())
    this.stage.remove()
    this.ring.remove()
  }

  // ── public controls ────────────────────────────────────────────────────
  /** Move the offset marker by a screen-space drag without starting a stroke. */
  moveMarkBy(dx: number, dy: number) {
    if (!this.touchDraws || !this.mark || this.floating) return
    this.mark = [this.mark[0] + dx / this.view.z, this.mark[1] + dy / this.view.z]
    this.updateMarkRing()
  }
  /** Go back to the previous tool (and make this one the new previous). */
  swapTool() {
    if (this.prevTool) this.setTool(this.prevTool)
  }
  setTool(t: UiTool) {
    if (t !== this.tool) {
      this.setMark(null) // changing tool leaves offset mode (the mark goes)
      this.prevTool = this.tool
    }
    this.tool = t
    this.hooks.onTool(t, this.prevTool)
    this.host.style.cursor = CURSORS[t]
    if (t !== 'eraser') this.ring.style.display = 'none'
    if (!isSelect(t)) this.clearSel()
    this.updateMarkRing()
  }
  setSetting(t: Tool, s: Partial<ToolSetting>) {
    Object.assign(this.settings[t], s)
    if (t === this.tool) this.updateMarkRing()
  }
  /** Hide this period to see what is underneath it. Earlier periods then show at full brightness; the timeline tool keeps working on
   *  them; any other tool touching the canvas shows this period again. */
  setHideCurrent(on: boolean) {
    if (this.hideCurrent === on) return
    this.hideCurrent = on
    this.clearSel()
    this.baseKey = ''
    this.rebuild()
  }
  /** Earlier periods underneath: dimmed (default) or at full brightness. */
  setDim(on: boolean) {
    this.dimLevel = on ? DIM : 1
    this.baseKey = ''
    this.rebuild()
  }
  /** Show / hide the overview of the whole canvas (this month plus the months shown underneath). */
  setMinimap(on: boolean) {
    this.miniOn = on
    this.mini.style.display = on ? 'block' : 'none'
    this.miniKey = ''
    this.drawMinimap()
  }
  setLayers(l: Layers) {
    this.layers = { ...l }
    this.sel = null
    this.rebuild()
  }
  setVisibleMonths(ms: string[]) {
    this.visibleMonths = ms.filter((m) => m !== this.month)
    this.data.watch(this.visibleMonths, this.month)
    this.rebuild()
  }
  get canUndo() {
    return this.log.canUndo
  }
  get canRedo() {
    return this.log.canRedo
  }
  undo() {
    if (!this.log.undo()) return
    this.sel = null
    this.data.added(this.month)
    this.hooks.onChange()
    this.rebuild()
  }
  redo() {
    if (!this.log.redo()) return
    this.sel = null
    this.data.added(this.month)
    this.hooks.onChange()
    this.rebuild()
  }
  /** The journal brought something new for `month` (a peer's ops, or ours coming back stamped). */
  remoteChanged(month: string) {
    if (month !== this.month && !this.visibleMonths.includes(month)) return
    if (month === this.month && this.stampAppended()) {
      this.refreshSelection()
      this.drawMinimap()
      this.hooks.onChange()
      return
    }
    if (month === this.month) this.refreshSelection()
    this.schedRebuild(30)
    this.hooks.onChange()
  }
  /** The common case for a peer's drawing: new strokes at the end of the canvas order, nothing else changed. Stamp just
   *  those onto what's already drawn. Returns false when it isn't that simple (an op landed earlier in the order, an
   *  undo / move / delete / image came in, the view moved) and a full rebuild is needed. */
  private stampAppended(): boolean {
    const bv = this.buf.bv
    if (this.lift || this.live || this.drag || this.rbTimer || bv.x !== this.view.x || bv.y !== this.view.y || bv.z !== this.view.z) return false
    const evs = this.events
    const now = evs.map(this.sig)
    if (this.stamped.length > now.length || !this.stamped.every((x, i) => x === now[i])) return false
    const extra = evs.slice(this.stamped.length)
    if (!extra.every((e) => e.type === 'stroke' && e.active !== false)) return false
    for (const e of extra) if (e.type === 'stroke') this.buf.stampStroke(e)
    this.buf.flush(this.ctx.ink, this.layers)
    this.stamped = now
    this.items = this.monthItems(this.month)
    return true
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
    for (const c of [this.base, this.imgs, this.ink, this.float, this.over, this.tmpInk, this.tmpMonth]) {
      c.width = W
      c.height = H
    }
    for (const c of [this.base, this.imgs, this.ink, this.float, this.over]) {
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
        this.bitmapsLoaded++
        this.schedRebuild(30)
      })
    }
    return null
  }

  private drawImages(ctx: CanvasRenderingContext2D, items: Item[]) {
    if (!this.layers.image) return
    const S = this.buf.bv.z * this.dpr
    for (const e of items) {
      if (e.tool !== 'image') continue
      const bmp = this.bitmapFor(e.hash)
      if (bmp) ctx.drawImage(bmp, (e.rect[0] - this.buf.bv.x) * S, (e.rect[1] - this.buf.bv.y) * S, e.rect[2] * S, e.rect[3] * S)
    }
  }

  /** Stamp the strokes that can reach the screen. A canvas this big is mostly off-screen: an item whose bounds miss the
   *  view contributes no pixels, so it isn't stamped at all (its dabs aren't even walked). */
  private stampAll(items: Item[]) {
    const { W, H, bv, dpr } = this.buf
    const vx0 = bv.x
    const vy0 = bv.y
    const vx1 = bv.x + W / (bv.z * dpr)
    const vy1 = bv.y + H / (bv.z * dpr)
    for (const e of items) {
      if (e.tool === 'image') continue
      let b = this.bboxCache.get(e.id)
      if (!b) {
        b = bboxOf([e])
        const m = e.size * 2.5 // a dab reaches ~3 sigma beyond the path
        b = [b[0] - m, b[1] - m, b[2] + m, b[3] + m]
        this.bboxCache.set(e.id, b)
      }
      if (b[2] < vx0 || b[0] > vx1 || b[3] < vy0 || b[1] > vy1) continue
      this.buf.stampStroke(e)
    }
  }
  private sig = (e: Ev) => `${e.id}:${e.active === false ? 0 : 1}`

  private monthItems(m: string) {
    const log = this.data.log(m)
    const lf = this.liftOthers.get(m)
    if (lf) return fold(log.events(), [{ type: 'delete', poly: lf.poly, only: lf.only, ids: lf.ids }]) // being dragged: draw it without the lifted ink
    let e = this.others.get(m)
    if (!e || e.v !== log.version) this.others.set(m, (e = { v: log.version, items: fold(log.events()) }))
    return e.items
  }

  rebuild() {
    window.clearTimeout(this.rbTimer)
    this.rbTimer = 0
    const { W, H } = this.buf
    if (!W) return
    this.view.z = clamp(this.view.z, 0.1, 20)
    this.hooks.onZoom(this.view.z)
    this.stage.style.transform = ''
    const { base, imgs, ink } = this.ctx
    // other months, oldest first, dimmed, underneath — redrawn only if something they depend on changed
    const months = [...this.visibleMonths].sort()
    const baseKey = [W, H, this.view.x, this.view.y, this.view.z, this.layers.pencil, this.layers.pen, this.layers.image, this.bitmapsLoaded, this.dimLevel, this.hideCurrent, [...this.liftOthers.keys()].join('+'), months.map((m) => m + ':' + this.data.log(m).version).join(',')].join('|')
    if (baseKey !== this.baseKey) {
      this.baseKey = baseKey
      base.clearRect(0, 0, W, H)
      const tm = this.tmpMonth.getContext('2d')!
      const ti = this.tmpInk.getContext('2d')!
      for (const m of months) {
        const evs = this.monthItems(m)
        this.buf.clear(this.view)
        this.stampAll(evs)
        tm.clearRect(0, 0, W, H)
        this.drawImages(tm, evs)
        ti.clearRect(0, 0, W, H)
        this.buf.flush(ti, this.layers) // only the part the strokes reached
        tm.drawImage(this.tmpInk, 0, 0)
        base.globalAlpha = this.hideCurrent ? 1 : this.dimLevel
        base.drawImage(this.tmpMonth, 0, 0)
        base.globalAlpha = 1
      }
    }
    // this month: images, then ink (the ink buffers stay live for incremental strokes)
    imgs.clearRect(0, 0, W, H)
    if (this.lift) this.items = fold(this.events, [{ type: 'delete', poly: this.lift.poly, only: this.lift.only, ids: this.lift.ids }])
    else this.items = this.monthItems(this.month)
    if (this.hideCurrent) this.items = [] // hidden: nothing of this period is drawn
    this.drawImages(imgs, this.items)
    this.buf.clear(this.view)
    this.stampAll(this.items)
    ink.clearRect(0, 0, W, H)
    this.buf.flush(ink, this.layers)
    this.stamped = this.lift ? [] : this.events.map(this.sig)
    this.ctx.float.clearRect(0, 0, W, H)
    this.float.style.transform = ''
    this.refreshSelectionView()
    this.updateMarkRing()
    this.drawOverlay()
    this.drawMinimap()
    this.saveView()
  }

  private schedRebuild(ms = 150) {
    window.clearTimeout(this.rbTimer)
    this.rbTimer = window.setTimeout(() => {
      if (this.live || this.floatDrag || this.selActive || [...this.ptrs.values()].some((p) => !p.pan)) return this.schedRebuild(250)
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
    const S = this.view.z * this.dpr
    o.strokeStyle = '#ff5200'
    o.fillStyle = '#ff5200'
    o.lineWidth = 1.5 * this.dpr
    const f = this.floating
    if (f) {
      const [x, y, w, h] = f.rect
      const dx = (x - this.view.x) * S
      const dy = (y - this.view.y) * S
      o.drawImage(f.bmp, dx, dy, w * S, h * S)
      o.setLineDash([6 * this.dpr, 4 * this.dpr])
      o.strokeRect(dx, dy, w * S, h * S)
      o.setLineDash([])
      o.beginPath()
      o.arc(dx + w * S, dy + h * S, 8 * this.dpr, 0, 6.3)
      o.fill()
    }
    const path = (poly: Poly, close: boolean) => {
      o.beginPath()
      poly.forEach((q, i) => (i ? o.lineTo((q[0] - this.view.x) * S, (q[1] - this.view.y) * S) : o.moveTo((q[0] - this.view.x) * S, (q[1] - this.view.y) * S)))
      if (close) o.closePath()
      o.stroke()
    }
    o.setLineDash([6 * this.dpr, 4 * this.dpr])
    if (this.lassoPath && this.lassoPath.length > 1) path(this.lassoPath, this.tool === 'rect')
    const sel = this.sel
    if (sel) {
      const xf = this.dragXf
      const poly = xf ? xfPoly(sel.poly, xf) : sel.poly
      const [bx0, by0] = xf ? xfXY(sel.bbox[0], sel.bbox[1], xf) : [sel.bbox[0], sel.bbox[1]]
      const [bx1, by1] = xf ? xfXY(sel.bbox[2], sel.bbox[3], xf) : [sel.bbox[2], sel.bbox[3]]
      o.globalAlpha = 0.5
      path(poly, true)
      o.globalAlpha = 1
      o.strokeRect((bx0 - this.view.x) * S, (by0 - this.view.y) * S, (bx1 - bx0) * S, (by1 - by0) * S)
      o.setLineDash([])
      o.beginPath()
      o.arc((bx1 - this.view.x) * S, (by1 - this.view.y) * S, 7 * this.dpr, 0, 6.3) // scale handle
      o.fill()
    }
    o.setLineDash([])
  }

  // ── selection (lasso / rect) ───────────────────────────────────────────
  private rectPoly(a: [number, number], b: [number, number]): Poly {
    return [a, [b[0], a[1]], b, [a[0], b[1]]]
  }
  private refreshSelectionView() {
    const b = this.sel?.bbox
    this.hooks.onSelection(b ? { x: (b[2] - this.view.x) * this.view.z, y: (b[1] - this.view.y) * this.view.z } : null)
  }
  clearSel() {
    if (!this.sel && !this.lassoPath) return
    this.sel = null
    this.lassoPath = null
    this.rectStart = null
    this.refreshSelectionView()
    this.drawOverlay()
  }
  /** The shape's ink in each earlier period that is showing (wipe tool): the same lasso, across time. */
  private partsIn(poly: Poly): SelPart[] {
    if (this.tool !== 'timeline') return []
    const parts: SelPart[] = []
    for (const m of [...this.visibleMonths].sort()) {
      const { pieces, ids } = piecesIn(this.monthItems(m), poly, this.layers)
      if (pieces.length) parts.push({ month: m, ids, pieces })
    }
    return parts
  }
  private setSel(poly: Poly, mine: { ids: string[]; pieces: Item[] }, others: SelPart[]) {
    if (!mine.pieces.length && !others.length) return this.clearSel()
    this.sel = { poly, ids: mine.ids, pieces: mine.pieces, others, bbox: bboxOf([...mine.pieces, ...others.flatMap((o) => o.pieces)]) }
    this.refreshSelectionView()
    this.drawOverlay()
  }
  private selectPoly(poly: Poly) {
    this.setSel(poly, this.hideCurrent ? { ids: [], pieces: [] } : piecesIn(fold(this.events), poly, this.layers), this.partsIn(poly))
  }
  /** The journal changed under a selection (our own op coming back stamped, or a peer's): keep holding the same ink.
   *  Its ids are stable, so look the pieces up again; the selection only goes if all of it is gone. */
  private refreshSelection() {
    const s = this.sel
    if (!s || this.drag) return
    const again = (items: Item[], ids: string[]) => {
      const want = new Set(ids)
      return piecesIn(items.filter((it) => want.has(it.id)), s.poly, this.layers).pieces
    }
    const pieces = again(fold(this.events), s.ids)
    const others = s.others.map((o) => ({ ...o, pieces: again(this.monthItems(o.month), o.ids) })).filter((o) => o.pieces.length)
    if (!pieces.length && !others.length) return this.clearSel()
    s.pieces = pieces
    s.others = others
    s.bbox = bboxOf([...pieces, ...others.flatMap((o) => o.pieces)])
    this.refreshSelectionView()
    this.drawOverlay()
  }
  /** After a move / duplicate: select exactly the pieces that op produced (in this period and, for the timeline tool, in each earlier
   *  one) — not whatever else sits where they landed. `exactIds` are items by their own id (new strokes added as copies). */
  private selectMoved(poly: Poly, opId: string | null, otherOps: { month: string; opId: string }[] = [], exactIds: string[] = []) {
    const pick = (items: Item[], id: string) => items.filter((it) => it.tool !== 'eraser' && it.id.includes(`~${id}i`) && this.layers[layerOf(it)])
    const want = new Set(exactIds)
    const mine = [...(opId ? pick(fold(this.events), opId) : []), ...fold(this.events).filter((it) => want.has(it.id) && this.layers[layerOf(it)])]
    const others = otherOps.map((o) => ({ month: o.month, pieces: pick(this.monthItems(o.month), o.opId) })).filter((o) => o.pieces.length).map((o) => ({ ...o, ids: o.pieces.map((it) => it.id) }))
    this.setSel(poly, { ids: mine.map((it) => it.id), pieces: mine }, others)
  }
  /** Add an event to this period's log without a live stroke behind it (a copy, not something just drawn). */
  private addCurrent(ev: Ev): string {
    const op = this.log.addEvent(ev)
    this.data.added(this.month)
    this.hooks.onChange()
    return op.cid
  }
  /** An earlier period's piece as a brand-new stroke / image event in this period, moved by `xf`. */
  private copyAsEvent(piece: Item, xf: Xf): Ev {
    const it = xfItem(piece, xf)
    if (it.tool === 'image') return { type: 'image', hash: it.hash, rect: it.rect.map((v) => +v.toFixed(1)) as ImageEv['rect'] }
    return { type: 'stroke', tool: it.tool, seed: it.seed, size: +it.size.toFixed(2), ...(it.op != null ? { op: it.op } : {}), pts: it.pts.map((q) => [+q[0].toFixed(1), +q[1].toFixed(1), q[2], q[3]] as Pt) }
  }
  /** Write an op into an earlier period's own log (earlier periods are read-only on the canvas, so this is how the wipe tool reaches them). */
  private addEarlier(month: string, ev: Ev): string {
    const op = this.data.log(month).addEvent(ev)
    this.data.added(month)
    return op.cid
  }
  /** Apply one op to this period (if the selection has ink here) and to each earlier period it reaches. Returns the op ids. */
  private applyToSelection(make: (ids: string[]) => Ev) {
    const s = this.sel!
    const mine = s.ids.length ? this.pushEvent(make(s.ids)).cid : null
    const others = s.others.map((o) => ({ month: o.month, opId: this.addEarlier(o.month, make(o.ids)) }))
    if (others.length) this.hooks.onChange()
    return { mine, others }
  }
  deleteSelection() {
    const s = this.sel
    if (!s) return
    if (s.others.length && !window.confirm(`Erase this from ${s.others.length + 1} periods? Erasing in earlier periods cannot be undone from here.`)) return
    const only = onlyList(this.layers)
    this.applyToSelection((ids) => ({ type: 'delete', poly: s.poly, only, ids }))
    this.sel = null
    this.refreshSelectionView()
    this.rebuild()
    this.drawOverlay()
  }
  /** Duplicate: this period's ink as before (a copy op); each earlier period's ink comes in as NEW strokes/images on the current timeline,
   *  not back into the period it came from. */
  duplicateSelection() {
    const s = this.sel
    if (!s) return
    const o = 24 / this.view.z
    const xf: Xf = { dx: o, dy: o, s: 1, cx: (s.bbox[0] + s.bbox[2]) / 2, cy: (s.bbox[1] + s.bbox[3]) / 2 }
    const only = onlyList(this.layers)
    const mine = s.ids.length ? this.pushEvent({ type: 'move', poly: s.poly, xf, copy: true, only, ids: s.ids }).cid : null
    const fresh = s.others.flatMap((p) => p.pieces).map((piece) => this.addCurrent(this.copyAsEvent(piece, xf)))
    this.sel = null
    this.rebuild()
    this.selectMoved(xfPoly(s.poly, xf), mine, [], fresh)
  }

  private selDown(e: PointerEvent) {
    this.settle()
    const [lx, ly] = this.local(e)
    const w = this.toWorld(lx, ly)
    const s = this.sel
    if (s) {
      const b = s.bbox
      const m = 8 / this.view.z
      const cx = (b[0] + b[2]) / 2
      const cy = (b[1] + b[3]) / 2
      if (Math.hypot(w[0] - b[2], w[1] - b[3]) <= 16 / this.view.z) return this.beginDrag('scale', w, cx, cy)
      if (w[0] >= b[0] - m && w[0] <= b[2] + m && w[1] >= b[1] - m && w[1] <= b[3] + m) return this.beginDrag('move', w, cx, cy)
      this.clearSel()
    }
    this.selActive = true
    this.rectStart = this.tool === 'rect' ? w : null
    this.lassoPath = this.tool === 'rect' ? this.rectPoly(w, w) : [w]
    this.drawOverlay()
  }

  /** Lift the selection off the canvas into its own layer; moving it afterwards is just a CSS transform. */
  private beginDrag(mode: 'move' | 'scale', w: [number, number], cx: number, cy: number) {
    const s = this.sel!
    this.selActive = true
    const only = onlyList(this.layers)
    this.lift = { poly: s.poly, only, ids: s.ids }
    this.liftOthers = new Map(s.others.map((o) => [o.month, { poly: s.poly, only, ids: o.ids }]))
    this.dragXf = { dx: 0, dy: 0, s: 1, cx, cy }
    this.drag = { mode, sx: w[0], sy: w[1], d0: Math.max(1e-6, Math.hypot(w[0] - cx, w[1] - cy)), cx, cy }
    this.rebuild() // main canvas without the selection
    // the lifted ink, drawn once into the floating layer: earlier periods' parts at their dimmed brightness, this period's on top
    const { W, H } = this.buf
    const f = this.ctx.float
    const tmp = this.tmpInk.getContext('2d')!
    f.clearRect(0, 0, W, H)
    const groups = [...s.others.map((o) => ({ pieces: o.pieces, alpha: this.hideCurrent ? 1 : this.dimLevel })), { pieces: s.pieces, alpha: 1 }]
    for (const g of groups) {
      if (!g.pieces.length) continue
      this.buf.clear(this.view)
      this.stampAll(g.pieces)
      this.buf.dirty = [0, 0, W, H]
      tmp.clearRect(0, 0, W, H)
      this.buf.flush(tmp, this.layers)
      f.globalAlpha = g.alpha
      f.drawImage(this.tmpInk, 0, 0)
      this.drawImages(f, g.pieces)
      f.globalAlpha = 1
    }
  }

  private selMove(e: PointerEvent) {
    const [lx, ly] = this.local(e)
    const w = this.toWorld(lx, ly)
    const d = this.drag
    const xf = this.dragXf
    if (d && xf) {
      if (d.mode === 'move') {
        xf.dx = w[0] - d.sx
        xf.dy = w[1] - d.sy
      } else xf.s = Math.max(0.1, Math.min(20, Math.hypot(w[0] - d.cx, w[1] - d.cy) / d.d0))
      const z = this.view.z
      const tx = z * ((xf.s - 1) * this.view.x + (1 - xf.s) * xf.cx + xf.dx)
      const ty = z * ((xf.s - 1) * this.view.y + (1 - xf.s) * xf.cy + xf.dy)
      this.float.style.transform = `matrix(${xf.s},0,0,${xf.s},${tx},${ty})`
      const b = this.sel!.bbox
      const [qx, qy] = xfXY(b[2], b[1], xf)
      this.hooks.onSelection({ x: (qx - this.view.x) * z, y: (qy - this.view.y) * z }) // the icons ride along
      this.drawOverlay()
    } else if (this.lassoPath) {
      if (this.rectStart) this.lassoPath = this.rectPoly(this.rectStart, w)
      else this.lassoPath.push(w)
      this.drawOverlay()
    }
  }

  private selUp() {
    this.selActive = false
    const xf = this.dragXf
    if (this.drag && xf && this.sel) {
      const { poly } = this.sel
      const only = this.lift?.only
      this.drag = null
      this.dragXf = null
      this.lift = null
      this.liftOthers = new Map()
      if (isIdentity(xf)) {
        this.rebuild()
        this.drawOverlay()
        return
      }
      const { mine, others } = this.applyToSelection((ids) => ({ type: 'move', poly, xf, only, ids }))
      this.rebuild()
      this.selectMoved(xfPoly(poly, xf), mine, others)
      return
    }
    const path = this.lassoPath
    this.lassoPath = null
    this.rectStart = null
    if (!path) return
    const poly = this.tool === 'rect' ? path : simplifyPoly(path, 1.5 / this.view.z)
    const [x0, y0, x1, y1] = [Math.min(...poly.map((p) => p[0])), Math.min(...poly.map((p) => p[1])), Math.max(...poly.map((p) => p[0])), Math.max(...poly.map((p) => p[1]))]
    if (poly.length < 3 || (x1 - x0) * this.view.z < 4 || (y1 - y0) * this.view.z < 4) return this.drawOverlay()
    this.selectPoly(poly)
  }

  // ── events ─────────────────────────────────────────────────────────────
  private pushEvent(ev: Ev) {
    const inStep = this.stamped.length === this.events.length // everything in the log is drawn
    const op = this.log.addEvent(ev)
    if (inStep && ev.type === 'stroke') this.stamped.push(`${op.cid}:1`) // the live stroke is already stamped
    this.data.added(this.month)
    this.hooks.onChange()
    this.drawMinimap()
    return op
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
    if (e.pointerType === 'touch' && this.mark) return // the mark ring shows where the tool is
    if (this.tool !== 'eraser' || this.floating) return void (this.ring.style.display = 'none')
    const d = 2 * 1.2 * this.settings.eraser.size * this.view.z
    const r = this.host.getBoundingClientRect()
    this.ring.style.cssText = `display:block;width:${d}px;height:${d}px;transform:translate(${e.clientX - r.left - d / 2}px,${e.clientY - r.top - d / 2}px);background:rgb(255 82 0 / ${0.06 + 0.3 * this.settings.eraser.op})`
  }

  /** Offset mode is "a mark exists": long press the canvas to enter (mark = where you pressed), tap to move the mark, drag to
   *  move the tool by the drag's offset from it, long press again to leave (clears the mark). Finger only. */
  private setMark(m: [number, number] | null) {
    const changed = Boolean(this.mark) !== Boolean(m)
    this.mark = m
    this.updateMarkRing()
    if (changed) this.hooks.onMark(Boolean(m))
  }
  /** The mark: a ring at the tool's offset position (eraser radius / pen width), persistent between touches. */
  private updateMarkRing() {
    const t = this.tool
    if (!isDraw(t) || !this.mark || this.floating) return void (this.ring.style.display = 'none')
    const z = this.view.z
    const d = t === 'eraser' ? 2 * 1.2 * this.settings.eraser.size * z : Math.max(12, this.settings[t].size * z)
    const x = (this.mark[0] - this.view.x) * z
    const y = (this.mark[1] - this.view.y) * z
    const fill = t === 'eraser' ? `rgb(255 82 0 / ${0.06 + 0.3 * this.settings.eraser.op})` : 'transparent'
    this.ring.style.cssText = `display:block;width:${d}px;height:${d}px;transform:translate(${x - d / 2}px,${y - d / 2}px);background:${fill}`
  }

  private barrel = false
  /** A pen's barrel button (pointer `buttons` bit 2), on its rising edge, swaps back to the previous tool. This is what a web
   *  page can see of a stylus button; the Apple Pencil Pro's squeeze is not exposed to web pages by Safari. */
  private penButton(e: PointerEvent) {
    if (e.pointerType !== 'pen') return
    const down = (e.buttons & 2) !== 0
    if (down && !this.barrel) this.swapTool()
    this.barrel = down
  }

  private pointerDown(e: PointerEvent) {
    if (this.hideCurrent && this.tool !== 'timeline' && e.button === 0) {
      this.setHideCurrent(false) // drawing or selecting here: bring this period back first
      this.hooks.onHideCurrent?.(false)
    }
    this.penButton(e)
    const now = performance.now()
    if (e.pointerType === 'touch' && this.penSeen && (this.live || now - this.lastPenT < 700)) return e.preventDefault() // palm
    try {
      this.host.setPointerCapture(e.pointerId)
    } catch {
      /* already cancelled */
    }
    if (e.pointerType === 'pen') {
      if (this.pend) window.clearTimeout(this.pend.timer)
      this.pend = null
      this.penSeen = true
      this.lastPenT = now
      for (const [id, q] of this.ptrs) if (q.pan) this.ptrs.delete(id)
      this.prevG = null
      if (this.touchDraws) {
        this.touchDraws = false
        this.hooks.onTouchDraws(false)
      }
    }
    const [lx, ly] = this.local(e)
    // a second finger means pan / pinch: drop whatever the first one had started, and navigate with both
    if (e.pointerType === 'touch' && [...this.ptrs.values()].some((p) => p.touch)) {
      this.abortInput()
      for (const p of this.ptrs.values()) if (p.touch) p.pan = true
      this.ptrs.set(e.pointerId, { x: lx, y: ly, pan: true, touch: true })
      this.prevG = null
      this.applyGesture()
      return e.preventDefault()
    }
    // mouse: right (or ctrl-) button, middle button, or space + drag pans; the primary button draws
    const mousePan = e.pointerType === 'mouse' && (e.button === 1 || e.button === 2 || e.ctrlKey)
    const draws = !(mousePan || this.spaceDown) && (e.pointerType !== 'touch' || this.touchDraws)
    if (!draws && e.pointerType === 'mouse') this.host.style.cursor = 'grabbing'
    if (mousePan && e.button !== 1) this.rightDown = { id: e.pointerId, x: e.clientX, y: e.clientY, moved: false }
    // lift-and-return: the previous stroke's pointerup hasn't arrived — finish it, never swallow the new one
    if (draws && this.live && (e.pointerType !== 'touch' || this.live.pid === e.pointerId)) {
      this.ptrs.delete(this.live.pid)
      this.endStroke()
    }
    this.ptrs.set(e.pointerId, { x: lx, y: ly, pan: !draws, touch: e.pointerType === 'touch' })
    if (draws && !this.live) {
      if (this.floating) this.floatDown(e)
      else if (isSelect(this.tool)) {
        this.selDown(e)
        if (e.pointerType === 'pen' && !this.drag) this.holdStart(e) // holding the pencil still with lasso / rect also goes back to the previous tool
      }
      else if ((e.pointerType === 'touch' || e.pointerType === 'pen') && isDraw(this.tool)) this.pendDown(e, lx, ly) // finger / pencil: wait to tell a drag, a tap and a long press apart
      else this.startStroke(e)
    } else {
      this.prevG = null
      this.applyGesture()
    }
    this.moveRing(e)
    e.preventDefault()
  }

  /** A finger touched down with a drawing tool. Not drawn yet: a drag starts the stroke (at the mark in offset mode, else where the
   *  finger landed), a tap moves the mark (offset mode) or leaves a dot, a long press enters / leaves offset mode. */
  private hold: { x: number; y: number; timer: number } | null = null
  private holdStart(e: PointerEvent) {
    this.holdClear()
    this.hold = {
      x: e.clientX,
      y: e.clientY,
      timer: window.setTimeout(() => {
        this.hold = null
        if (!this.selActive || this.drag) return
        this.lassoPath = null // nothing was being outlined: it was a hold, not a lasso
        this.rectStart = null
        this.selActive = false
        this.drawOverlay()
        navigator.vibrate?.(15)
        this.swapTool()
      }, 600),
    }
  }
  private holdClear() {
    if (this.hold) window.clearTimeout(this.hold.timer)
    this.hold = null
  }

  private pendDown(e: PointerEvent, lx: number, ly: number) {
    this.pend = { qx: lx, qy: ly, e0: e, done: false, timer: window.setTimeout(() => this.longPress(), e.pointerType === 'pen' ? 600 : 500) }
  }
  private longPress() {
    const p = this.pend
    if (!p || p.done) return
    p.done = true
    navigator.vibrate?.(15)
    if (p.e0.pointerType === 'pen') return this.swapTool() // hold the pencil still: back to the previous tool
    this.setMark(this.mark ? null : this.toWorld(p.qx, p.qy))
  }
  private pendMove(e: PointerEvent) {
    const p = this.pend!
    if (p.done) return
    const [lx, ly] = this.local(e)
    const pen = p.e0.pointerType === 'pen'
    if (Math.hypot(lx - p.qx, ly - p.qy) < (pen ? 4 : 6)) return
    window.clearTimeout(p.timer)
    this.pend = null
    if (this.mark && !pen) {
      this.off = { qx: lx, qy: ly, started: true }
      this.startStroke(e) // first point = the mark itself
      this.updateMarkRing()
    } else {
      this.startStroke(p.e0) // from where the finger landed, then on to here
      this.addPoints(e)
    }
  }
  private pendUp() {
    const p = this.pend!
    window.clearTimeout(p.timer)
    this.pend = null
    if (p.done) return
    if (this.mark && p.e0.pointerType === 'touch') this.setMark(this.toWorld(p.qx, p.qy)) // tap: move the mark, draw nothing
    else {
      this.startStroke(p.e0) // plain tap: a dot
      this.endStroke()
    }
  }

  /** Throw away an unfinished stroke / lasso / selection drag (nothing was committed, so a rebuild restores the canvas). */
  private abortInput() {
    this.holdClear()
    if (this.pend) window.clearTimeout(this.pend.timer)
    this.pend = null
    this.off = null
    if (this.live) {
      this.live = null
      this.rebuild()
    }
    if (this.selActive) {
      this.selActive = false
      this.lassoPath = null
      this.rectStart = null
      if (this.drag) {
        this.drag = null
        this.dragXf = null
        this.lift = null
        this.liftOthers = new Map()
        this.rebuild()
      }
      this.drawOverlay()
    }
    this.floatDrag = null
  }

  private pointerMove(e: PointerEvent) {
    if (this.hold && Math.hypot(e.clientX - this.hold.x, e.clientY - this.hold.y) > 4) this.holdClear() // it moved: a lasso, not a hold
    this.penButton(e)
    this.moveRing(e)
    const p = this.ptrs.get(e.pointerId)
    if (!p) return
    const [lx, ly] = this.local(e)
    p.x = lx
    p.y = ly
    if (e.pointerType === 'pen') this.lastPenT = performance.now()
    const rd = this.rightDown
    if (rd && rd.id === e.pointerId && !rd.moved && Math.hypot(e.clientX - rd.x, e.clientY - rd.y) > 5) rd.moved = true
    if (p.pan) this.applyGesture()
    else if (this.pend) this.pendMove(e)
    else if (this.live) {
      this.addPoints(e)
      if (this.off) this.updateMarkRing()
    }
    else if (this.floatDrag) this.floatMove(e)
    else if (this.selActive) this.selMove(e)
  }

  private pointerUp(e: PointerEvent) {
    if (e.pointerType === 'pen') this.lastPenT = performance.now()
    const p = this.ptrs.get(e.pointerId)
    if (!p) return
    this.holdClear()
    this.ptrs.delete(e.pointerId)
    if (p.pan) this.prevG = null
    else if (this.pend) this.pendUp()
    else if (this.live) this.endStroke()
    else if (this.floatDrag) {
      this.floatDrag = null
      this.rebuild()
    } else if (this.selActive) this.selUp()
    if (this.rightDown?.id === e.pointerId) {
      if (!this.rightDown.moved && e.type === 'pointerup') this.hooks.onContext(e.clientX, e.clientY)
      this.rightDown = null
    }
    if (p.pan && e.pointerType === 'mouse') this.host.style.cursor = this.spaceDown ? 'grab' : CURSORS[this.tool]
    if (e.pointerType === 'touch') {
      if (this.mark) this.updateMarkRing()
      else this.ring.style.display = 'none'
    }
  }

  // ── strokes ────────────────────────────────────────────────────────────
  private startStroke(e: PointerEvent) {
    this.settle()
    const t = this.tool as Tool
    const s = this.settings[t]
    const seed = (Math.random() * 4294967296) >>> 0
    const st = { tool: t, seed, size: s.size, op: s.op < 1 ? s.op : undefined, only: t === 'eraser' ? onlyList(this.layers) : undefined } // an eraser only reaches the layers that are visible now
    this.live = { ...st, pts: [], stamper: new Stamper(this.buf, st), pid: e.pointerId, lastT: 0, lastX: 0, lastY: 0 }
    this.addPoints(e, true)
  }

  private pointFrom(e: PointerEvent, first: boolean): Pt {
    const live = this.live!
    const [lx, ly] = this.local(e)
    let [wx, wy] = this.toWorld(lx, ly)
    const o = this.off
    if (o?.started && this.mark) {
      // offset mode: the tool moves by the finger's drag, from wherever the mark was
      wx = this.mark[0] + (lx - o.qx) / this.view.z
      wy = this.mark[1] + (ly - o.qy) / this.view.z
      o.qx = lx
      o.qy = ly
      this.mark = [wx, wy]
    }
    let p = e.pressure
    let tilt = 0
    if (e.pointerType !== 'pen') {
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
    this.off = null
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
      if (live.only) ev.only = live.only
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

  // ── minimap ────────────────────────────────────────────────────────────
  private miniGo(e: PointerEvent) {
    const r = this.mini.getBoundingClientRect()
    const m = this.miniMap
    const wx = m.minX + (e.clientX - r.left - m.offX) / m.scale
    const wy = m.minY + (e.clientY - r.top - m.offY) / m.scale
    this.view.x = wx - this.host.clientWidth / this.view.z / 2
    this.view.y = wy - this.host.clientHeight / this.view.z / 2
    this.gestureTransform()
  }

  /** An overview of every month on show: strokes as thin lines, erasers cutting through them, images as pictures or
   *  boxes, and the part you're looking at as an outline. Click or drag to go there. */
  private drawMinimap() {
    if (!this.miniOn) return
    const dpr = this.dpr
    const cw = this.mini.clientWidth || 200
    const ch = this.mini.clientHeight || 140
    if (this.mini.width !== Math.round(cw * dpr)) {
      this.mini.width = this.miniLayer.width = Math.round(cw * dpr)
      this.mini.height = this.miniLayer.height = Math.round(ch * dpr)
      this.miniKey = ''
    }
    const groups = [...this.visibleMonths].sort().map((m) => ({ items: this.monthItems(m), dim: true, v: this.data.log(m).version })).concat(this.hideCurrent ? [] : [{ items: this.monthItems(this.month), dim: false, v: this.log.version }])
    // bounds: all the ink (stable while you pan), recomputed only when a month's log changes
    const bkey = groups.map((g) => g.v).join(',') + '|' + this.layers.image
    if (this.miniBounds.key !== bkey) {
      let a = 1e9
      let b = 1e9
      let c = -1e9
      let d = -1e9
      for (const g of groups) {
        const real = g.items.filter((it) => it.tool !== 'eraser')
        if (!real.length) continue
        const bb = bboxOf(real)
        a = Math.min(a, bb[0])
        b = Math.min(b, bb[1])
        c = Math.max(c, bb[2])
        d = Math.max(d, bb[3])
      }
      this.miniBounds = { key: bkey, b: a > c ? null : [a, b, c, d] }
    }
    let [x0, y0, x1, y1] = this.miniBounds.b ?? [1e9, 1e9, -1e9, -1e9]
    if (x0 > x1) {
      x0 = this.view.x
      y0 = this.view.y
      x1 = this.view.x + this.host.clientWidth / this.view.z
      y1 = this.view.y + this.host.clientHeight / this.view.z
    }
    const pad = Math.max(x1 - x0, y1 - y0, 1) * 0.06
    x0 -= pad
    y0 -= pad
    x1 += pad
    y1 += pad
    const scale = Math.min(cw / (x1 - x0), ch / (y1 - y0))
    const offX = (cw - (x1 - x0) * scale) / 2
    const offY = (ch - (y1 - y0) * scale) / 2
    this.miniMap = { minX: x0, minY: y0, scale, offX, offY }
    const key = [groups.map((g) => g.v).join(','), this.layers.pencil, this.layers.pen, this.layers.image, [x0, y0, x1, y1].map((v) => v.toFixed(1)).join(','), cw, ch].join('|')
    if (key !== this.miniKey) {
      this.miniKey = key
      const L = this.miniLayer.getContext('2d')!
      L.setTransform(1, 0, 0, 1, 0, 0)
      L.clearRect(0, 0, this.miniLayer.width, this.miniLayer.height)
      const T = document.createElement('canvas')
      T.width = this.miniLayer.width
      T.height = this.miniLayer.height
      const t = T.getContext('2d')!
      for (const g of groups) {
        t.setTransform(1, 0, 0, 1, 0, 0)
        t.clearRect(0, 0, T.width, T.height)
        t.setTransform(dpr * scale, 0, 0, dpr * scale, dpr * (offX - x0 * scale), dpr * (offY - y0 * scale))
        t.lineCap = t.lineJoin = 'round'
        for (const it of g.items) {
          if (it.tool === 'image') {
            if (!this.layers.image) continue
            const bmp = this.bitmaps.get(it.hash)
            if (bmp && bmp !== 'loading') t.drawImage(bmp, it.rect[0], it.rect[1], it.rect[2], it.rect[3])
            else {
              t.strokeStyle = 'rgb(255 82 0 / 0.6)'
              t.lineWidth = 1 / scale
              t.strokeRect(it.rect[0], it.rect[1], it.rect[2], it.rect[3])
            }
            continue
          }
          const layer = layerOf(it)
          if (it.tool === 'eraser' ? false : !this.layers[layer]) continue
          t.globalCompositeOperation = it.tool === 'eraser' ? 'destination-out' : 'source-over'
          t.globalAlpha = it.tool === 'eraser' ? it.op ?? 1 : it.tool === 'pencil' ? 0.55 * (it.op ?? 1) : it.op ?? 1
          t.strokeStyle = '#ff5200'
          t.lineWidth = it.tool === 'eraser' ? 2 * it.size * 1.2 : Math.max(it.size * (it.tool === 'pen' ? 1 : 0.8), 1.5 / scale)
          t.beginPath()
          it.pts.forEach((q, i) => (i ? t.lineTo(q[0], q[1]) : t.moveTo(q[0], q[1])))
          if (it.pts.length === 1) t.lineTo(it.pts[0][0] + 0.01, it.pts[0][1])
          t.stroke()
        }
        t.globalCompositeOperation = 'source-over'
        t.globalAlpha = 1
        L.globalAlpha = g.dim ? 0.35 : 1
        L.drawImage(T, 0, 0)
      }
      L.globalAlpha = 1
    }
    const c = this.mini.getContext('2d')!
    c.setTransform(1, 0, 0, 1, 0, 0)
    c.clearRect(0, 0, this.mini.width, this.mini.height)
    c.drawImage(this.miniLayer, 0, 0)
    c.setTransform(dpr, 0, 0, dpr, 0, 0)
    c.strokeStyle = '#ff5200'
    c.lineWidth = 1.5
    c.strokeRect(offX + (this.view.x - x0) * scale, offY + (this.view.y - y0) * scale, (this.host.clientWidth / this.view.z) * scale, (this.host.clientHeight / this.view.z) * scale)
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
  /** Scroll zooms (about the pointer); panning is right-button / middle / space drag, or fingers. Ctrl+wheel (a trackpad
   *  pinch) zooms faster per delta. */
  private wheel(e: WheelEvent) {
    e.preventDefault()
    const r = this.host.getBoundingClientRect()
    const dy = e.deltaMode === 1 ? e.deltaY * 16 : e.deltaY
    this.zoomAround(e.clientX - r.left, e.clientY - r.top, Math.exp(-dy * (e.ctrlKey ? 0.01 : 0.0015)))
  }
  /** Zoom by `factor`, keeping the world point under host-px (cx, cy) where it is. */
  private zoomAround(cx: number, cy: number, factor: number) {
    const nz = clamp(this.view.z * factor, 0.1, 20)
    const wx = this.view.x + cx / this.view.z
    const wy = this.view.y + cy / this.view.z
    this.view.z = nz
    this.view.x = wx - cx / nz
    this.view.y = wy - cy / nz
    this.gestureTransform()
  }
  /** Zoom buttons: step in / out about the middle of the screen. */
  zoomBy(factor: number) {
    this.zoomAround(this.host.clientWidth / 2, this.host.clientHeight / 2, factor)
  }
  /** Back to 100 % about the middle of the screen. */
  resetZoom() {
    this.zoomBy(1 / this.view.z)
  }
  /** During a gesture the existing pixels just move (CSS); the sharp replay follows once it settles. */
  private gestureTransform() {
    const bv = this.buf.bv
    const s = this.view.z / bv.z
    const tx = this.view.z * (bv.x - this.view.x)
    const ty = this.view.z * (bv.y - this.view.y)
    this.stage.style.transform = `translate(${tx}px,${ty}px) scale(${s})`
    this.hooks.onZoom(this.view.z)
    this.refreshSelectionView()
    this.updateMarkRing()
    this.drawMinimap()
    this.schedRebuild(180)
  }

  private key(e: KeyboardEvent, down: boolean) {
    if (/^(INPUT|TEXTAREA|SELECT)$/.test((e.target as HTMLElement)?.tagName)) return
    if (e.code === 'Space') {
      this.spaceDown = down
      if (!this.ptrs.size) this.host.style.cursor = down ? 'grab' : CURSORS[this.tool] // space + drag pans: show the hand
    }
    if (!down) return
    const mod = e.metaKey || e.ctrlKey
    if (mod && e.key === 'z') {
      e.preventDefault()
      e.shiftKey ? this.redo() : this.undo()
    } else if (mod && (e.key === 'y' || e.key === 'Y')) {
      e.preventDefault()
      this.redo()
    } else if (mod && (e.key === 'd' || e.key === 'D') && this.sel) {
      e.preventDefault()
      this.duplicateSelection()
    } else if (!mod && !e.altKey && e.key.toLowerCase() === 'c' && this.sel) {
      e.preventDefault()
      this.duplicateSelection() // left-hand twins of Cmd+D / Delete
    } else if (!mod && !e.altKey && e.key.toLowerCase() === 'q' && this.sel) {
      e.preventDefault()
      this.deleteSelection()
    } else if ((e.key === 'Delete' || e.key === 'Backspace') && this.sel) {
      e.preventDefault()
      this.deleteSelection()
    } else if (e.key === 'Escape' && this.sel) this.clearSel()
    else if (this.floating && e.key === 'Enter') this.placeImage()
    else if (this.floating && e.key === 'Escape') this.cancelImage()
  }
}
