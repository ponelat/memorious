/** Browser-local persistence for the month canvases (first cut — not yet in the journal).
 *  Events per month live in localStorage; image pixels are content-addressed blobs in IndexedDB. */
import type { Ev } from './ink'

const EV_PREFIX = 'memorious.canvas.events.'
const monthRe = /^\d{4}-\d{2}$/

export const monthKey = (d = new Date()) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}`

export function prevMonth(key: string): string {
  const [y, m] = key.split('-').map(Number)
  return m === 1 ? `${y - 1}-12` : `${y}-${String(m - 1).padStart(2, '0')}`
}

export function monthLabel(key: string): string {
  const [y, m] = key.split('-').map(Number)
  return new Date(y, m - 1, 1).toLocaleDateString(undefined, { month: 'short', year: 'numeric' })
}

export const isMonthKey = (s: string) => monthRe.test(s)

export function loadMonth(key: string): Ev[] {
  try {
    const raw = localStorage.getItem(EV_PREFIX + key)
    return raw ? (JSON.parse(raw) as Ev[]) : []
  } catch {
    return []
  }
}

/** Months that have any drawing, newest first. */
export function monthsWithContent(): string[] {
  const out: string[] = []
  for (let i = 0; i < localStorage.length; i++) {
    const k = localStorage.key(i)
    if (k?.startsWith(EV_PREFIX)) {
      const m = k.slice(EV_PREFIX.length)
      if (isMonthKey(m) && loadMonth(m).some((e) => e.active !== false)) out.push(m)
    }
  }
  return out.sort().reverse()
}

const timers = new Map<string, number>()
const pending = new Map<string, Ev[]>()
function write(key: string) {
  const evs = pending.get(key)
  if (!evs) return
  pending.delete(key)
  window.clearTimeout(timers.get(key))
  timers.delete(key)
  try {
    localStorage.setItem(EV_PREFIX + key, JSON.stringify(evs))
  } catch {
    /* quota: the canvas keeps working in memory */
  }
}
/** Debounced (a stroke end used to stall iPad when it stringified synchronously); flushed when the page hides. */
export function saveMonth(key: string, evs: Ev[]) {
  pending.set(key, evs)
  if (!timers.has(key)) timers.set(key, window.setTimeout(() => write(key), 600))
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
