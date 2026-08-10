import { useCallback, useEffect, useRef, useState } from 'react'
import { api, Entry } from '../api'
import { CaptureBar } from '../components/CaptureBar'
import { EntryRow, PhotoRun } from '../components/EntryRow'
import { Lightbox } from '../components/Lightbox'

/** Consecutive photos within this gap collapse into one polaroid fan. */
const PHOTO_RUN_GAP_MS = 30 * 60 * 1000

type StreamItem =
  | { type: 'day'; key: string; label: string }
  | { type: 'entry'; key: string; entry: Entry }
  | { type: 'photos'; key: string; entries: Entry[] }

function dayLabel(ms: number): string {
  const d = new Date(ms)
  const today = new Date()
  const yesterday = new Date(today.getTime() - 86_400_000)
  const sameDay = (a: Date, b: Date) => a.toDateString() === b.toDateString()
  if (sameDay(d, today)) return 'today'
  if (sameDay(d, yesterday)) return 'yesterday'
  return d.toLocaleDateString(undefined, {
    weekday: 'short',
    day: 'numeric',
    month: 'short',
    year: d.getFullYear() === today.getFullYear() ? undefined : 'numeric',
  })
}

/** All grouping is derived here, at render time — nothing is stored. */
export function deriveStream(entries: Entry[]): StreamItem[] {
  const items: StreamItem[] = []
  let currentDay = ''
  let run: Entry[] = []

  const flushRun = () => {
    if (run.length === 0) return
    if (run.length === 1) items.push({ type: 'entry', key: run[0].event_id, entry: run[0] })
    else items.push({ type: 'photos', key: run[0].event_id, entries: run })
    run = []
  }

  for (const e of entries) {
    const day = new Date(e.recorded_at).toDateString()
    if (day !== currentDay) {
      flushRun()
      currentDay = day
      items.push({ type: 'day', key: `day-${day}`, label: dayLabel(e.recorded_at) })
    }
    if (e.kind === 'photo') {
      const prev = run[run.length - 1]
      if (prev && prev.recorded_at - e.recorded_at > PHOTO_RUN_GAP_MS) flushRun()
      run.push(e)
    } else {
      flushRun()
      items.push({ type: 'entry', key: e.event_id, entry: e })
    }
  }
  flushRun()
  return items
}

export function StreamView() {
  const [entries, setEntries] = useState<Entry[]>([])
  const [nextBefore, setNextBefore] = useState<number | null>(null)
  const [query, setQuery] = useState('')
  const [results, setResults] = useState<Entry[] | null>(null)
  const [lightbox, setLightbox] = useState<Entry[] | null>(null)
  const sentinel = useRef<HTMLDivElement>(null)
  const loading = useRef(false)

  const loadMore = useCallback(async (before?: number) => {
    if (loading.current) return
    loading.current = true
    try {
      const page = await api.feed(before)
      setEntries((cur) => {
        const seen = new Set(cur.map((e) => e.event_id))
        const fresh = page.entries.filter((e) => !seen.has(e.event_id))
        return before === undefined && cur.length === 0 ? page.entries : [...cur, ...fresh]
      })
      setNextBefore(page.entries.length === 0 ? null : page.next_before)
    } finally {
      loading.current = false
    }
  }, [])

  const refresh = useCallback(async () => {
    const page = await api.feed()
    setEntries(page.entries)
    setNextBefore(page.next_before)
  }, [])

  useEffect(() => {
    refresh().catch(() => {})
  }, [refresh])

  // Infinite scroll.
  useEffect(() => {
    const el = sentinel.current
    if (!el || nextBefore === null) return
    const obs = new IntersectionObserver((io) => {
      if (io.some((i) => i.isIntersecting)) loadMore(nextBefore)
    })
    obs.observe(el)
    return () => obs.disconnect()
  }, [nextBefore, loadMore])

  // Search.
  useEffect(() => {
    if (query.trim() === '') {
      setResults(null)
      return
    }
    const t = setTimeout(() => {
      api.search(query).then(setResults).catch(() => setResults([]))
    }, 250)
    return () => clearTimeout(t)
  }, [query])

  const onCaptured = useCallback(
    (entry: Entry) => setEntries((cur) => [entry, ...cur]),
    [],
  )

  const onRedact = useCallback(async (entry: Entry) => {
    await api.redact(entry.event_id)
    setEntries((cur) => cur.filter((e) => e.event_id !== entry.event_id))
    setLightbox(null)
  }, [])

  const shown = results ?? entries
  const items = deriveStream(shown)

  return (
    <main className="stream">
      <CaptureBar onCaptured={onCaptured} />
      <input
        className="search"
        type="search"
        placeholder="search"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
      />
      {results && <p className="hint">{results.length} result{results.length === 1 ? '' : 's'}</p>}
      <ol className="entries">
        {items.map((item) => {
          if (item.type === 'day') {
            return (
              <li key={item.key} className="day">
                {item.label}
              </li>
            )
          }
          if (item.type === 'photos') {
            return (
              <li key={item.key}>
                <PhotoRun entries={item.entries} onOpen={() => setLightbox(item.entries)} />
              </li>
            )
          }
          return (
            <li key={item.key}>
              <EntryRow
                entry={item.entry}
                onRedact={onRedact}
                onOpenPhoto={() => setLightbox([item.entry])}
              />
            </li>
          )
        })}
      </ol>
      {shown.length === 0 && !results && <p className="hint">nothing yet — write something</p>}
      <div ref={sentinel} className="sentinel" />
      {lightbox && <Lightbox entries={lightbox} onClose={() => setLightbox(null)} onRedact={onRedact} />}
    </main>
  )
}
