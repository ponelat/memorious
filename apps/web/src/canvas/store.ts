/** Browser-local persistence for the month canvases: a cache of each month's op log (so the canvas works offline
 *  and resumes after a reload) — the journal, via /api/ink, is the source of truth when the host has one.
 *  Image pixels are content-addressed blobs in IndexedDB. */
import type { Ev } from './ink'
import type { Op } from './oplog'

const OPS_PREFIX = 'memorious.canvas.ops.'
const LEGACY_PREFIX = 'memorious.canvas.events.' // before ink synced: Ev[] with `active` flags, per local month
export { isMonthKey, monthKey, monthLabel, prevMonth, weekKey } from './period'
import { isMonthKey } from './period'

export interface SavedLog {
  v: 1
  /** This device's id as the journal knows it ('local' when there is no journal). */
  me: string
  /** The server cursor this cache is current to (0 = never pulled). */
  cursor: number
  ops: Op[]
}

export function loadOps(month: string): SavedLog | null {
  try {
    const raw = localStorage.getItem(OPS_PREFIX + month)
    return raw ? (JSON.parse(raw) as SavedLog) : null
  } catch {
    return null
  }
}

/** Strokes drawn before ink synced, as ops (undone ones dropped). */
export function legacyOps(month: string, me: string): Op[] {
  try {
    const raw = localStorage.getItem(LEGACY_PREFIX + month)
    if (!raw) return []
    const t = Date.now()
    return (JSON.parse(raw) as Ev[])
      .filter((e) => e.active !== false)
      .flatMap((e, i): Op[] => {
        const base = { cid: crypto.randomUUID(), dev: me, t: t + i }
        switch (e.type) {
          case 'stroke':
            return [{ ...base, data: { op: 'stroke', tool: e.tool, seed: e.seed, size: e.size, ...(e.op != null ? { opacity: e.op } : {}), pts: e.pts } }]
          case 'image':
            return [{ ...base, data: { op: 'image', hash: e.hash, rect: e.rect } }]
          case 'move':
            return [{ ...base, data: { op: 'move', poly: e.poly, xf: e.xf, ...(e.copy ? { copy: true } : {}), ...(e.only ? { only: e.only } : {}) } }]
          case 'delete':
            return [{ ...base, data: { op: 'delete', poly: e.poly, ...(e.only ? { only: e.only } : {}) } }]
        }
      })
  } catch {
    return []
  }
}

/** Periods (weeks and legacy months) with a cached log here, newest first. */
export function monthsWithContent(): string[] {
  const out: string[] = []
  for (let i = 0; i < localStorage.length; i++) {
    const k = localStorage.key(i)
    if (k?.startsWith(OPS_PREFIX)) {
      const m = k.slice(OPS_PREFIX.length)
      if (isMonthKey(m)) out.push(m) // key scan only: parsing every month's log on each render was the expensive part
    }
  }
  return out.sort().reverse()
}

const timers = new Map<string, number>()
const pending = new Map<string, SavedLog>()
function write(key: string) {
  const v = pending.get(key)
  if (!v) return
  pending.delete(key)
  window.clearTimeout(timers.get(key))
  timers.delete(key)
  try {
    localStorage.setItem(OPS_PREFIX + key, JSON.stringify(v))
  } catch {
    /* quota: the canvas keeps working in memory and from the journal */
  }
}
/** Debounced (a stroke end used to stall iPad when it stringified synchronously); flushed when the page hides. */
export function saveOps(month: string, v: SavedLog) {
  pending.set(month, v)
  if (!timers.has(month)) timers.set(month, window.setTimeout(() => write(month), 600))
}
const flushAll = () => [...pending.keys()].forEach(write)
window.addEventListener('pagehide', flushAll)
document.addEventListener('visibilitychange', () => document.hidden && flushAll())

// ── image blobs (IndexedDB) ────────────────────────────────────────────────
const db = new Promise<IDBDatabase>((res, rej) => {
  const r = indexedDB.open('memorious-canvas', 1)
  r.onupgradeneeded = () => r.result.createObjectStore('blobs')
  r.onsuccess = () => res(r.result)
  r.onerror = () => rej(r.error)
})
const op = <T,>(mode: IDBTransactionMode, fn: (s: IDBObjectStore) => IDBRequest<T>) =>
  db.then(
    (d) =>
      new Promise<T>((res, rej) => {
        const t = d.transaction('blobs', mode)
        const q = fn(t.objectStore('blobs'))
        t.oncomplete = () => res(q.result)
        t.onerror = () => rej(t.error)
      }),
  )

async function sha(buf: ArrayBuffer) {
  const h = await crypto.subtle.digest('SHA-256', buf)
  return [...new Uint8Array(h)].slice(0, 12).map((b) => b.toString(16).padStart(2, '0')).join('')
}

export async function putBlob(blob: Blob): Promise<string> {
  const bytes = await blob.arrayBuffer()
  const hash = await sha(bytes)
  if (!(await op('readonly', (s) => s.count(hash)))) await op('readwrite', (s) => s.put({ bytes, type: blob.type }, hash))
  return hash
}

export async function getBlob(hash: string): Promise<Blob | null> {
  const r = (await op('readonly', (s) => s.get(hash))) as { bytes: ArrayBuffer; type: string } | undefined
  return r ? new Blob([r.bytes], { type: r.type }) : null
}

/** Downscale + lossy WebP: doodles and pasted pictures don't need more. */
export async function ingest(file: Blob, maxDim = 1024, q = 0.7) {
  const bmp = await createImageBitmap(file)
  const k = Math.min(1, maxDim / Math.max(bmp.width, bmp.height))
  const w = Math.max(1, Math.round(bmp.width * k))
  const h = Math.max(1, Math.round(bmp.height * k))
  const c = document.createElement('canvas')
  c.width = w
  c.height = h
  c.getContext('2d')!.drawImage(bmp, 0, 0, w, h)
  const blob = await new Promise<Blob>((r) => c.toBlob((b) => r(b!), 'image/webp', q))
  const out = await createImageBitmap(blob)
  return { hash: await putBlob(blob), bmp: out, w, h }
}

export function loadJSON<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key)
    return raw ? { ...fallback, ...JSON.parse(raw) } : fallback
  } catch {
    return fallback
  }
}
export const saveJSON = (key: string, v: unknown) => {
  try {
    localStorage.setItem(key, JSON.stringify(v))
  } catch {
    /* ignore */
  }
}
