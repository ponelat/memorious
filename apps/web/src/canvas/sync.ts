/** Keeps each month's `MonthLog` and the journal in step (docs/CANVAS-SYNC.md).
 *
 *  Local-first: ops are applied and cached at once; they are pushed in batches ~0.6 s after the last one (and on
 *  page hide), and the journal is polled for what other peers drew — fast for the current month, slowly for the
 *  months shown underneath. Each poll first pulls then pushes, so an op whose push response was lost is
 *  recognised by its cid on the way back instead of being sent twice. Without `api.ink` (a host that doesn't have
 *  it yet) the canvas simply stays on this device. */
import type { InkApi } from '../api/types'
import { MonthLog, toWire } from './oplog'
import { legacyOps, loadOps, saveOps } from './store'

const K_DEVICE = 'memorious.canvas.device'
const PUSH_DEBOUNCE = 600
const POLL_CURRENT = 2500
const POLL_OTHER = 20000
const POLL_STREAMING = 60000 // while the event stream is up, polling is just a safety net
const BATCH = 100

interface Entry {
  log: MonthLog
  cursor: number
  busy: boolean
  lastPull: number
  dirtyAt: number // when the oldest unpushed op was added; 0 = nothing waiting
  failures: number
  retryAt: number // don't try again before this (backoff after a failure)
  again: boolean // an announcement arrived while a sync was running: go round once more
}

export class CanvasData {
  private entries = new Map<string, Entry>()
  private watched = new Set<string>()
  private current = ''
  private timer = 0
  private me = localStorage.getItem(K_DEVICE) ?? 'local'
  private streaming = false
  private closeStream: (() => void) | null = null
  private reconnect = 0
  private reconnectFails = 0

  constructor(
    private ink: InkApi | undefined,
    /** A month's log changed because of the journal (a peer's ops, or ours coming back stamped). */
    private onRemote: (month: string) => void,
  ) {
    this.timer = window.setInterval(() => this.tick(), 500)
    window.addEventListener('pagehide', this.onHide)
    document.addEventListener('visibilitychange', this.onHide)
    this.connect()
  }

  get synced() {
    return !!this.ink
  }

  dispose() {
    window.clearInterval(this.timer)
    window.clearTimeout(this.reconnect)
    this.closeStream?.()
    window.removeEventListener('pagehide', this.onHide)
    document.removeEventListener('visibilitychange', this.onHide)
  }

  /** Listen for ink as it lands (server-sent events) instead of polling; reconnect with backoff, polling in the meantime. */
  private connect() {
    if (!this.ink?.subscribe) return
    this.closeStream = this.ink.subscribe(
      (e) => {
        if (e.type === 'ready') {
          this.streaming = true
          this.reconnectFails = 0
          for (const m of this.watched) this.pullSoon(m) // catch up on anything missed while it was down
        } else if (e.type === 'resync') for (const m of this.watched) this.pullSoon(m)
        else if (e.type === 'ink' && e.month) this.pullSoon(e.month)
      },
      () => {
        this.streaming = false
        this.closeStream = null
        this.reconnectFails++
        window.clearTimeout(this.reconnect)
        this.reconnect = window.setTimeout(() => this.connect(), Math.min(30000, 1000 * 2 ** Math.min(this.reconnectFails, 5)))
      },
    )
  }
  /** Pull a month now (right after a peer's ink was announced), or once more as soon as the running sync ends. */
  private pullSoon(month: string) {
    const e = this.entries.get(month)
    if (!e || !this.watched.has(month)) return
    if (e.busy) e.again = true
    else void this.sync(month, e)
  }

  private onHide = () => {
    if (document.visibilityState === 'hidden' || document.visibilityState === undefined) void this.flush()
  }

  /** The log for a month, loading its cache (and any pre-sync local drawing) on first use. */
  log(month: string): MonthLog {
    return this.entry(month).log
  }

  private entry(month: string): Entry {
    let e = this.entries.get(month)
    if (e) return e
    const saved = loadOps(month)
    const log = saved ? MonthLog.restore(month, saved.me, saved.ops) : new MonthLog(month, this.me)
    if (!saved) {
      for (const o of legacyOps(month, this.me)) log.ops.push(o)
      log.version++
    }
    e = { log, cursor: saved?.cursor ?? 0, busy: false, lastPull: 0, dirtyAt: log.pending().length ? Date.now() : 0, failures: 0, retryAt: 0, again: false }
    this.entries.set(month, e)
    if (!saved && log.ops.length) this.persist(month, e)
    return e
  }

  private persist(month: string, e: Entry) {
    saveOps(month, { v: 1, me: e.log.me, cursor: e.cursor, ops: e.log.ops })
  }

  /** Which months to keep polling; `current` is the one being drawn on. */
  watch(months: string[], current: string) {
    this.current = current
    this.watched = new Set([current, ...months])
    for (const m of this.watched) {
      const e = this.entry(m)
      e.lastPull = 0 // pull soon
    }
  }

  /** An op was added to `month`'s log: cache it and schedule the push. */
  added(month: string) {
    const e = this.entry(month)
    if (!e.dirtyAt) e.dirtyAt = Date.now()
    this.persist(month, e)
  }

  async remoteMonths(): Promise<string[]> {
    try {
      return (await this.ink?.months()) ?? []
    } catch {
      return []
    }
  }

  private tick() {
    if (!this.ink || document.hidden) return
    const now = Date.now()
    for (const [month, e] of this.entries) {
      if (e.busy || now < e.retryAt) continue
      const pushDue = e.dirtyAt && now - e.dirtyAt >= PUSH_DEBOUNCE
      const poll = this.watched.has(month) ? (this.streaming ? POLL_STREAMING : month === this.current ? POLL_CURRENT : POLL_OTHER) : Infinity
      if (pushDue || now - e.lastPull >= poll) void this.sync(month, e)
    }
  }

  /** Push everything pending, right now (page hide). */
  async flush() {
    if (!this.ink) return
    await Promise.all([...this.entries].filter(([, e]) => e.log.pending().length && !e.busy).map(([m, e]) => this.push(m, e).catch(() => {})))
  }

  private async sync(month: string, e: Entry) {
    e.busy = true
    try {
      await this.pull(month, e)
      await this.push(month, e)
      e.failures = 0
      e.retryAt = 0
    } catch {
      e.failures++
      e.retryAt = Date.now() + Math.min(30000, 2 ** Math.min(e.failures, 5) * 1000) // back off
    } finally {
      e.busy = false
      if (e.again) {
        e.again = false
        void this.sync(month, e)
      }
    }
  }

  private async pull(month: string, e: Entry) {
    if (!this.ink) return
    let r = await this.ink.list(month, e.cursor || undefined)
    if (e.log.me !== 'local' && r.device_id !== e.log.me && e.cursor) {
      // this cache belongs to another journal: start from the top (merging dedupes by cid)
      e.cursor = 0
      r = await this.ink.list(month)
    }
    if (r.device_id !== e.log.me) {
      e.log.adoptDevice(r.device_id)
      this.me = r.device_id
      localStorage.setItem(K_DEVICE, r.device_id)
    }
    const changed = e.log.merge(r.events)
    e.cursor = r.cursor
    e.lastPull = Date.now()
    if (changed || r.events.length) {
      this.persist(month, e)
      if (changed) this.onRemote(month)
    }
  }

  private async push(month: string, e: Entry) {
    if (!this.ink) return
    let pending = e.log.pending()
    if (!pending.length) {
      e.dirtyAt = 0
      return
    }
    while (pending.length) {
      const chunk = pending.slice(0, BATCH)
      const res = await this.ink.append(month, chunk.map(toWire))
      chunk.forEach((o, i) => res[i] && e.log.confirm(o.cid, res[i].event_id, res[i].recorded_at))
      pending = e.log.pending()
    }
    e.dirtyAt = 0
    this.persist(month, e)
    this.onRemote(month) // order may have changed now ops are stamped
  }
}
