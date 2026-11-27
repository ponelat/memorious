/** A month's canvas as an append-only log of ops (docs/CANVAS-SYNC.md). Pure logic, no I/O: the sync loop
 *  feeds it what the server has and drains what is still pending.
 *
 *  Every op has a client-made `cid`. Undo / redo are ops that name a `cid`, so they work before the server
 *  has assigned an event id, and a retry is recognised instead of applied twice. Canvas order is
 *  (recorded_at, event_id) — identical on every peer — with this device's unconfirmed ops after everything
 *  confirmed. */
import type { Ev, Poly, Pt, Tool, Xf } from './ink'

export type OpData =
  | { op: 'stroke'; tool: Tool; seed: number; size: number; opacity?: number; only?: string[]; pts: Pt[] }
  | { op: 'image'; hash: string; rect: [number, number, number, number] }
  | { op: 'move'; poly: Poly; xf: Xf; copy?: boolean; only?: string[]; ids?: string[] }
  | { op: 'delete'; poly: Poly; only?: string[]; ids?: string[] }
  | { op: 'undo'; target: string }
  | { op: 'redo'; target: string }

export interface Op {
  cid: string
  /** Author device id. */
  dev: string
  /** recorded_at once confirmed; creation time while pending. */
  t: number
  /** Server event id; undefined while pending. */
  eid?: string
  data: OpData
}

/** One ink event as /api/ink returns it. */
export interface WireRecord {
  event_id: string
  device_id: string
  seq?: number
  recorded_at: number
  data: Record<string, unknown> & { cid?: string }
}

const KNOWN = new Set(['stroke', 'image', 'move', 'delete', 'undo', 'redo'])
const isDrawable = (d: OpData) => d.op !== 'undo' && d.op !== 'redo'

const uid = () => (typeof crypto !== 'undefined' && 'randomUUID' in crypto ? crypto.randomUUID() : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`)

const byCanvasOrder = (a: Op, b: Op) => a.t - b.t || (a.eid ?? '').localeCompare(b.eid ?? '')

/** The in-memory Ev (what `fold` reads) <-> the wire op. Opacity is `op` in memory but `opacity` on the wire, since `op` names the op. */
export function toData(ev: Ev): OpData {
  switch (ev.type) {
    case 'stroke': {
      const d: OpData = { op: 'stroke', tool: ev.tool, seed: ev.seed, size: ev.size, pts: ev.pts }
      if (ev.op != null) d.opacity = ev.op
      if (ev.only) d.only = ev.only
      return d
    }
    case 'image':
      return { op: 'image', hash: ev.hash, rect: ev.rect }
    case 'move': {
      const d: OpData = { op: 'move', poly: ev.poly, xf: ev.xf }
      if (ev.copy) d.copy = true
      if (ev.only) d.only = ev.only
      if (ev.ids) d.ids = ev.ids
      return d
    }
    case 'delete': {
      const d: OpData = { op: 'delete', poly: ev.poly }
      if (ev.only) d.only = ev.only
      if (ev.ids) d.ids = ev.ids
      return d
    }
  }
}

function toEv(d: OpData, active: boolean, id: string): Ev | null {
  switch (d.op) {
    case 'stroke':
      return { type: 'stroke', id, tool: d.tool, seed: d.seed, size: d.size, op: d.opacity, only: d.only, pts: d.pts, active }
    case 'image':
      return { type: 'image', id, hash: d.hash, rect: d.rect, active }
    case 'move':
      return { type: 'move', id, poly: d.poly, xf: d.xf, copy: d.copy, only: d.only, ids: d.ids, active }
    case 'delete':
      return { type: 'delete', id, poly: d.poly, only: d.only, ids: d.ids, active }
    default:
      return null
  }
}

export class MonthLog {
  ops: Op[] = []
  /** Bumped on every change, so consumers can cache what they derive. */
  version = 0
  private resolved: { v: number; events: Ev[]; active: Map<string, boolean> } | null = null

  constructor(
    readonly month: string,
    public me: string,
  ) {}

  static restore(month: string, me: string, ops: Op[]): MonthLog {
    const l = new MonthLog(month, me)
    l.ops = ops.map((o) => ({ ...o }))
    l.resort()
    return l
  }

  private resort() {
    const confirmed = this.ops.filter((o) => o.eid !== undefined).sort(byCanvasOrder)
    const pending = this.ops.filter((o) => o.eid === undefined)
    this.ops = [...confirmed, ...pending]
    this.version++
  }

  pending(): Op[] {
    return this.ops.filter((o) => o.eid === undefined)
  }

  add(data: OpData): Op {
    const op: Op = { cid: uid(), dev: this.me, t: Date.now(), data }
    this.ops.push(op)
    this.version++
    return op
  }
  addEvent(ev: Ev): Op {
    return this.add(toData(ev))
  }

  /** The server stored our op: stamp it. */
  confirm(cid: string, eid: string, t: number) {
    const o = this.ops.find((x) => x.cid === cid)
    if (!o || o.eid) return
    o.eid = eid
    o.t = t
    this.resort()
  }

  /** Fold server records in. Returns whether anything visible changed. */
  merge(records: WireRecord[]): boolean {
    let changed = false
    // one index per merge: a period now keeps every op (no folds), and a find() per record made a poll O(n²)
    const byCid = new Map<string, Op>()
    for (const o of this.ops) byCid.set(o.cid, o)
    for (const r of records) {
      const raw = r.data ?? {}
      if (typeof raw.op !== 'string' || !KNOWN.has(raw.op)) continue
      const cid = typeof raw.cid === 'string' ? raw.cid : r.event_id // ops from clients that predate cids
      const have = byCid.get(cid)
      const { cid: _c, ...data } = raw
      void _c
      if (have) {
        if (!have.eid) {
          have.eid = r.event_id
          have.t = r.recorded_at
          have.dev = r.device_id
          changed = true
        }
        continue
      }
      const op: Op = { cid, dev: r.device_id, t: r.recorded_at, eid: r.event_id, data: data as OpData }
      this.ops.push(op)
      byCid.set(cid, op)
      changed = true
    }
    if (changed) this.resort()
    return changed
  }

  /** Ev list for `fold`, with undo/redo applied. */
  events(): Ev[] {
    return this.resolve().events
  }

  private resolve() {
    if (this.resolved?.v === this.version) return this.resolved
    const active = new Map<string, boolean>()
    for (const o of this.ops) if (isDrawable(o.data)) active.set(o.cid, true)
    for (const o of this.ops) {
      if (o.data.op === 'undo' && active.has(o.data.target)) active.set(o.data.target, false)
      else if (o.data.op === 'redo' && active.has(o.data.target)) active.set(o.data.target, true)
    }
    const events: Ev[] = []
    for (const o of this.ops) {
      if (!isDrawable(o.data)) continue
      const ev = toEv(o.data, active.get(o.cid) ?? true, o.cid)
      if (ev) events.push(ev)
    }
    this.resolved = { v: this.version, events, active }
    return this.resolved
  }

  private ownActiveDrawables(): Op[] {
    const { active } = this.resolve()
    return this.ops.filter((o) => o.dev === this.me && isDrawable(o.data) && active.get(o.cid))
  }

  get canUndo(): boolean {
    return this.ownActiveDrawables().length > 0
  }

  undo(): Op | null {
    const last = this.ownActiveDrawables().at(-1)
    return last ? this.add({ op: 'undo', target: last.cid }) : null
  }

  /** My undone ops, most recent first, that a redo could bring back: replay my ops — any new drawing ends the chain. */
  private redoStack(): string[] {
    const stack: string[] = []
    for (const o of this.ops) {
      if (o.dev !== this.me) continue
      if (isDrawable(o.data)) stack.length = 0
      else if (o.data.op === 'undo') stack.push(o.data.target)
      else if (o.data.op === 'redo') {
        const i = stack.lastIndexOf(o.data.target)
        if (i >= 0) stack.splice(i, 1)
      }
    }
    return stack
  }

  get canRedo(): boolean {
    const { active } = this.resolve()
    const top = this.redoStack().at(-1)
    return top !== undefined && active.get(top) === false
  }

  redo(): Op | null {
    const top = this.redoStack().at(-1)
    const { active } = this.resolve()
    return top !== undefined && active.get(top) === false ? this.add({ op: 'redo', target: top }) : null
  }

  /** After learning this device's real id, claim the ops made before we knew it. */
  adoptDevice(id: string) {
    if (id === this.me) return
    for (const o of this.ops) if (o.dev === this.me) o.dev = id
    this.me = id
    this.version++
  }

  get hasContent(): boolean {
    return this.resolve().events.some((e) => e.active !== false)
  }
}

/** The wire shape of an op: its data plus the cid. */
export const toWire = (o: Op): Record<string, unknown> => ({ ...o.data, cid: o.cid })
