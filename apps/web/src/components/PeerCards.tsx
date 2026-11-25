import { useMemo, useState, type ReactNode } from 'react'
import type { NewestEvent, PeerHistory, PeerInfo, SlotRuns, Status } from '../api'

/**
 * The Peers list: one card per peer. Quick view = last successful contact (big), the newest event here against the newest
 * event it held at that contact, and a 7-day chart of when each side was alive and when contact failed. Tap for the
 * detailed view. Colours/contrast live in `.pc` (styles.css); nothing here is orange-on-white text.
 */

const SLOT_MIN = 5
const BUCKET = 6 // slots per drawn bucket: data is 5-minute slots, drawn as 30-minute buckets
const AGE_OK = 30 * 60_000
const AGE_STALE = 24 * 3600_000
const WINDOW_MS = 7 * 24 * 3600_000

function ago(ms: number, now: number): string {
  const s = Math.max(0, Math.round((now - ms) / 1000))
  if (s < 5) return 'just now'
  if (s < 90) return `${s} s ago`
  if (s < 5400) return `${Math.round(s / 60)} min ago`
  if (s < 172800) return `${Math.round(s / 3600)} h ago`
  return `${Math.round(s / 86400)} days ago`
}

function dur(min: number): string {
  if (min >= 120) return `${(min / 60).toFixed(min % 60 ? 1 : 0)} h`
  return `${min} min`
}

const KIND_ICON: Record<string, string> = {
  text: 'M6 8h12M6 12h12M6 16h8',
  photo: 'M4 7h4l2-2h4l2 2h4v12H4zM12 10a3.5 3.5 0 100 7 3.5 3.5 0 000-7z',
  voice: 'M12 4a3 3 0 013 3v5a3 3 0 01-6 0V7a3 3 0 013-3zM6 11a6 6 0 0012 0M12 17v3',
  music: 'M9 18V6l10-2v12M9 18a2.5 2.5 0 11-5 0 2.5 2.5 0 015 0zM19 16a2.5 2.5 0 11-5 0 2.5 2.5 0 015 0z',
  video: 'M4 6h11v12H4zM15 10l5-3v10l-5-3',
  ink: 'M4 19c3-9 6 1 8-6s5-4 8-8',
}
const DEVICE_ICON = 'M5 5h14v5H5zM5 14h14v5H5zM8 7.5h.01M8 16.5h.01'

function Icon({ d, size = 18 }: { d: string; size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d={d} />
    </svg>
  )
}

function EventLine({ label, ev, now, none }: { label: string; ev?: NewestEvent | null; now: number; none: string }) {
  return (
    <>
      <span className="pc-lab">{label}</span>
      {ev ? (
        <>
          <span className="pc-tt"><Icon d={KIND_ICON[ev.kind] ?? KIND_ICON.text} size={16} /> {ev.title}</span>
          <span className="pc-tm">{ago(ev.recorded_at, now)}</span>
        </>
      ) : (
        <>
          <span className="pc-tt pc-mute">{none}</span>
          <span className="pc-tm">—</span>
        </>
      )}
    </>
  )
}

/** Expand absolute slot runs into a per-slot flag over the window. */
function expand(runs: SlotRuns, from: number, n: number): Uint8Array {
  const out = new Uint8Array(n)
  for (const [s, l] of runs) for (let i = Math.max(s, from); i < s + l && i < from + n; i++) out[i - from] = 1
  return out
}

function buckets(flags: Uint8Array): boolean[] {
  const out: boolean[] = []
  for (let i = 0; i < flags.length; i += BUCKET) {
    let any = false
    for (let j = i; j < Math.min(i + BUCKET, flags.length); j++) if (flags[j]) any = true
    out.push(any)
  }
  return out
}

function lineRuns(b: boolean[]): [number, number][] {
  const out: [number, number][] = []
  let start = -1
  b.forEach((v, i) => {
    if (v && start < 0) start = i
    if (!v && start >= 0) {
      out.push([start, i])
      start = -1
    }
  })
  if (start >= 0) out.push([start, b.length])
  return out
}

export interface ChartStats {
  reachedPct: number | null
  longestMin: number
  lastLinkedSlot: number | null
}

/** The 7-day chart + its numbers, from one peer's history. */
export function useChart(h: PeerHistory | undefined) {
  return useMemo(() => {
    if (!h) return null
    const n = h.to - h.from + 1
    const mine = expand(h.mine, h.from, n)
    const theirs = expand(h.theirs, h.from, n)
    const linked = expand(h.linked, h.from, n)
    const failed = expand(h.failed, h.from, n)
    const red = new Uint8Array(n)
    let attemptsOk = 0
    let attemptsBad = 0
    let lastLinked: number | null = null
    for (let i = 0; i < n; i++) {
      if (linked[i]) {
        attemptsOk++
        lastLinked = h.from + i
      }
      if (failed[i] && mine[i] && theirs[i]) {
        red[i] = 1
        attemptsBad++
      }
    }
    const redB = buckets(red)
    let best = 0
    let run = 0
    for (const v of redB) {
      run = v ? run + 1 : 0
      best = Math.max(best, run)
    }
    const stats: ChartStats = {
      reachedPct: attemptsOk + attemptsBad ? Math.round((attemptsOk / (attemptsOk + attemptsBad)) * 100) : null,
      longestMin: best * BUCKET * SLOT_MIN,
      lastLinkedSlot: lastLinked,
    }
    return { n, mine: buckets(mine), theirs: buckets(theirs), red: redB, stats, from: h.from }
  }, [h])
}

function AliveChart({ h, now, label }: { h: PeerHistory | undefined; now: number; label: string }) {
  const c = useChart(h)
  if (!c) return null
  const W = 800
  const nb = c.mine.length
  const X = (i: number) => (i * W) / nb
  const seg = (runs: [number, number][], y: number, cls: string) =>
    runs.map(([a, b]) => <line key={`${cls}${a}`} className={cls} x1={X(a)} x2={Math.max(X(b), X(a) + 1.5)} y1={y} y2={y} vectorEffect="non-scaling-stroke" />)
  const days = [...Array(7)].map((_, k) => new Date(now - (6 - k) * 864e5).toLocaleDateString(undefined, { weekday: 'short' }))
  const noContact = c.stats.lastLinkedSlot == null
  const mx = c.stats.lastLinkedSlot != null ? ((c.stats.lastLinkedSlot + 1 - c.from) * W) / c.n : null
  return (
    <div className="pc-graph">
      <div className="pc-legend">
        <span><i className="mac" />this device up</span>
        <span><i className="peer" />{label} up</span>
        <span><i className="red" />{label} up, unreachable</span>
      </div>
      <div className="pc-chart">
        <div className="pc-ylab"><span style={{ top: '35%' }}>this device</span><span style={{ top: '62%' }}>{label}</span></div>
        <div className="pc-plot">
          <svg viewBox="0 0 800 40" preserveAspectRatio="none" role="img" aria-label={`7-day alive history for ${label}`}>
            {[...Array(8)].map((_, k) => <line key={k} className="pc-day" x1={(k * W) / 7} x2={(k * W) / 7} y1="0" y2="40" vectorEffect="non-scaling-stroke" />)}
            {seg(lineRuns(c.mine), 14, 'pc-l-mac')}
            {seg(lineRuns(c.theirs), 25, 'pc-l-peer')}
            {lineRuns(c.red).map(([a, b]) => <rect key={`r${a}`} className="pc-l-red" x={X(a)} y="19" width={Math.max(X(b) - X(a), 3)} height="12" rx="2" />)}
            {mx != null && <line className="pc-now" x1={mx} x2={mx} y1="0" y2="40" vectorEffect="non-scaling-stroke" />}
          </svg>
          {noContact && <div className="pc-nocontact">no contact in 7 days</div>}
        </div>
      </div>
      <div className="pc-days">{days.map((d, i) => <span key={i}>{d}</span>)}</div>
      <div className="pc-cstat">
        {noContact ? (
          <span>never connected in 7 days</span>
        ) : (
          <>
            <span>{c.stats.reachedPct != null ? <>reached on <b>{c.stats.reachedPct}%</b> of pings</> : 'no failed pings'}</span>
            <span>longest unreachable <b>{c.stats.longestMin ? dur(c.stats.longestMin) : 'none'}</b></span>
          </>
        )}
      </div>
    </div>
  )
}

function Meter({ parts }: { parts: [number, string][] }) {
  const total = parts.reduce((a, [n]) => a + n, 0) || 1
  return (
    <div className="pc-meter">
      {parts.map(([n, cls], i) => <i key={i} className={cls} style={{ width: `${(n / total) * 100}%` }} />)}
    </div>
  )
}

export interface PeerCardsProps {
  status: Status
  history?: PeerHistory[]
  now: number
  nameOf: (peer: PeerInfo) => string
  /** The editable name + required toggle + device facts, supplied by the host view (it owns those widgets). */
  renderTools: (peer: PeerInfo) => ReactNode
  buildText: (peer: PeerInfo) => string | null
  onPing: () => void
  pinging: boolean
}

export function PeerCards({ status, history, now, nameOf, renderTools, buildText, onPing, pinging }: PeerCardsProps) {
  const [open, setOpen] = useState<Set<string>>(new Set())
  const peers = status.peers ?? []
  const hist = new Map((history ?? []).map((h) => [h.endpoint_id, h]))
  const total = status.events_total ?? 0
  const mediaTotal = status.media?.referenced ?? 0

  const sorted = [...peers].sort((a, b) => b.last_ok_ms - a.last_ok_ms)
  return (
    <div className="pc">
      {sorted.map((p) => {
        const name = nameOf(p)
        const age = now - p.last_ok_ms
        const inWindow = p.last_ok_ms > 0 && age <= WINDOW_MS
        const tone = p.last_ok_ms <= 0 || age > AGE_STALE ? 'bad' : age > AGE_OK ? 'warn' : 'ok'
        const tag =
          p.last_ok_ms <= 0 ? 'never reached'
          : !inWindow ? `last heard ${ago(p.last_ok_ms, now)}`
          : (p.events_missing ?? 0) > 0 ? `${p.events_missing} behind`
          : age > AGE_OK ? 'may have more'
          : ''
        const isOpen = open.has(p.endpoint_id)
        const toggle = () => setOpen((s) => { const n = new Set(s); n.has(p.endpoint_id) ? n.delete(p.endpoint_id) : n.add(p.endpoint_id); return n })
        const m = p.media
        const behind = p.events_missing ?? 0
        const heldEv = p.events_held ?? 0
        const build = buildText(p)
        const label = name.length > 9 ? `${name.slice(0, 8)}…` : name
        return (
          <article key={p.endpoint_id} className={`pc-card${isOpen ? ' open' : ''}`}>
            <button className="pc-main" onClick={toggle} aria-expanded={isOpen}>
              <span className="pc-ico"><Icon d={DEVICE_ICON} size={24} /></span>
              <span className="pc-name">
                <b>{name}</b>
                <span className={`pc-chip${p.required !== false ? ' req' : ''}`}>{p.required !== false ? 'Required' : 'Optional'}</span>
              </span>
              <span className="pc-lp">
                <small>Last ping</small>
                <b className={`t-${tone}`}>{p.last_ok_ms <= 0 ? 'never' : inWindow ? ago(p.last_ok_ms, now) : 'none in 7 days'}</b>
                {tag && <span className={`t-${tone === 'ok' ? 'warn' : tone}`}>{tag}</span>}
              </span>
            </button>
            <div className="pc-ev">
              <EventLine label="Newest here" ev={status.newest_here} now={now} none="nothing yet" />
              <EventLine label="Newest it held" ev={p.newest_held} now={now} none="nothing reported yet" />
            </div>
            <AliveChart h={hist.get(p.endpoint_id)} now={now} label={label} />
            <button className="pc-more" onClick={toggle}>
              <span>{isOpen ? 'Hide details' : 'Details'}</span>
              <span className="pc-chev">›</span>
            </button>
            {isOpen && (
              <div className="pc-det">
                <div className="pc-cols">
                  <div>
                    <section>
                      <h4>Data</h4>
                      <div className="pc-lbl"><span>Events</span><span className="pc-mono">{heldEv.toLocaleString()} / {total.toLocaleString()}{behind > 0 && <> · <b className="t-warn">{behind} behind</b></>}</span></div>
                      <Meter parts={[[heldEv, 'ok'], [Math.max(0, total - heldEv), 'none']]} />
                      {m && (
                        <>
                          <div className="pc-lbl" style={{ marginTop: 14 }}>
                            <span>Media</span>
                            <span className="pc-mono">
                              {m.held} held{m.evictable ? ` · ${m.evictable} by policy` : ''}{m.lost ? ` · ${m.lost} lost` : ''}
                              {(p.media_missing ?? 0) > 0 && <> · <b className="t-warn">{p.media_missing} missing</b></>}
                            </span>
                          </div>
                          <Meter parts={[[m.held, 'ok'], [m.evictable, 'soft'], [m.lost ?? 0, 'lost'], [p.media_missing ?? 0, 'warn'], [Math.max(0, mediaTotal - m.referenced), 'none']]} />
                        </>
                      )}
                    </section>
                    <section>
                      <h4>Role</h4>
                      {renderTools(p)}
                    </section>
                  </div>
                  <div>
                    <section>
                      <h4>Connection</h4>
                      <ul className="pc-methods">
                        {p.conn ? (
                          <li><b>{p.conn.transport === 'relay' ? 'Relay' : p.conn.lan ? 'Local network' : 'Direct'}</b><span>{p.conn.detail}</span><em>IN USE</em></li>
                        ) : (
                          <li><b>No live path</b><span>between contacts</span></li>
                        )}
                        <li><b>Found by</b><span>{p.discovery === 'ticket' ? 'pairing ticket' : p.discovery === 'inbound' ? 'it found us' : 'unknown'}</span></li>
                      </ul>
                      <div className="pc-actions"><button className="pc-btn primary" onClick={onPing} disabled={pinging}>{pinging ? 'Pinging…' : 'Ping now'}</button></div>
                    </section>
                    <section>
                      <h4>Identity</h4>
                      <dl className="pc-kv">
                        {build && (<><dt>Build</dt><dd>{build}</dd></>)}
                        {p.device_id && (<><dt>Device id</dt><dd className="pc-mono">{p.device_id}</dd></>)}
                        <dt>Endpoint</dt><dd className="pc-mono">{p.endpoint_id.slice(0, 16)}…</dd>
                      </dl>
                    </section>
                  </div>
                </div>
              </div>
            )}
          </article>
        )
      })}
    </div>
  )
}
