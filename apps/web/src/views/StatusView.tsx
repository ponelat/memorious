import { useEffect, useState } from 'react'
import { api, DownloadFile, NetConfig, PeerInfo, PeerPing, Status } from '../api'
import { agoLabel, edgeKind, MapPeer, PeerMap } from '../components/PeerMap'

function prettySize(bytes: number): string {
  if (bytes > 1_000_000_000) return `${(bytes / 1_000_000_000).toFixed(2)} GB`
  if (bytes > 1_000_000) return `${(bytes / 1_000_000).toFixed(1)} MB`
  if (bytes > 1_000) return `${Math.round(bytes / 1_000)} kB`
  return `${bytes} B`
}

/** "memorious-desktop-macos-arm64.zip" → "journal desktop · macos arm64" */
function prettyName(name: string): string {
  return name.replace(/\.(zip|dmg|tar\.gz)$/, '').replace(/-/g, ' ')
}

function prettyDate(ms: number): string {
  return new Date(ms).toLocaleDateString(undefined, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
  })
}

function shortId(id: string): string {
  // uuidv7 device ids share their (timestamp) prefix — the tail tells them apart.
  return `${id.slice(0, 8)}…${id.slice(-4)}`
}

function trunc(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n - 1)}…` : s
}

/** A row on the devices list: this device, a live peer, or a device we only
 * know from the event log (no direct contact yet). */
interface DeviceRow {
  key: string
  deviceId?: string
  name?: string
  isSelf: boolean
  peer?: PeerInfo
  headSeq?: number
}

function deviceRows(status: Status): DeviceRow[] {
  const names = status.names ?? {}
  const rows: DeviceRow[] = [
    {
      key: status.device_id,
      deviceId: status.device_id,
      name: names[status.device_id],
      isSelf: true,
      headSeq: status.heads[status.device_id],
    },
  ]
  const mapped = new Set([status.device_id])
  for (const peer of status.peers ?? []) {
    const deviceId = peer.device_id ?? undefined
    if (deviceId) mapped.add(deviceId)
    rows.push({
      key: peer.endpoint_id,
      deviceId,
      name: deviceId ? names[deviceId] : undefined,
      isSelf: false,
      peer,
      headSeq: deviceId ? status.heads[deviceId] : undefined,
    })
  }
  for (const deviceId of Object.keys(status.heads)) {
    if (mapped.has(deviceId)) continue
    rows.push({ key: deviceId, deviceId, name: names[deviceId], isSelf: false, headSeq: status.heads[deviceId] })
  }
  return rows
}

/** What a peer holds, as of our last handshake with it: events as x of y
 * (version vectors, so "behind" is exact), media by its own tally — a gap
 * its retention policy excuses is named, not counted against it. */
function holdingsLabel(peer: PeerInfo, status: Status): { text: string; behind: boolean } {
  if (peer.events_held === null || peer.events_held === undefined) {
    return { text: 'holds: unknown — synced before this build', behind: false }
  }
  const total = status.events_total ?? Object.values(status.heads).reduce((a, b) => a + b, 0)
  const missing = peer.events_missing ?? 0
  let text = `holds ${peer.events_held} of ${total} events`
  text += missing > 0 ? ` · ${missing} behind` : ' · complete'
  let behind = missing > 0
  const m = peer.media
  if (m) {
    const mediaMissing = peer.media_missing ?? 0
    text += ` · media ${m.held} of ${m.referenced}`
    if (mediaMissing > 0) {
      text += ` · ${mediaMissing} missing`
      behind = true
    } else if (m.evictable > 0) {
      text += ` · ${m.evictable} let go by its policy`
    }
  }
  return { text, behind }
}

/** The two facts per peer: how it was discovered, and the data transport in
 * use right now (with any proxy in the chain called out). */
function discoveryLabel(peer: PeerInfo): string {
  if (peer.discovery === 'ticket') return 'discovered: pairing ticket'
  if (peer.discovery === 'inbound') return 'discovered: it found us'
  return 'discovered: before this build'
}

function transportLabel(peer: PeerInfo): string {
  const kind = edgeKind(peer)
  if (kind === 'lan') return `transport: direct QUIC on the LAN (${peer.conn!.detail}) · p2p, no proxy`
  if (kind === 'internet') return `transport: direct QUIC over the internet (${peer.conn!.detail}) · p2p, no proxy`
  if (kind === 'relay') return `transport: via public relay ${peer.conn!.detail} · proxied`
  return `transport: idle — last sync ${agoLabel(peer.last_ok_ms)}`
}

/** The probe verdict, spelled out: reachable (with round trip) or not (why). */
function PingLine({ ping }: { ping: PeerPing }) {
  if (ping.ok) {
    return <span className="ping-ok">ping: reachable · {ping.rtt_ms} ms — able to sync</span>
  }
  return <span className="ping-fail">ping: unreachable — {ping.error ?? 'no answer'}</span>
}

function NameEditor({ deviceId, name, onSaved }: { deviceId: string; name?: string; onSaved: () => void }) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(name ?? '')
  const [err, setErr] = useState<string | null>(null)

  async function save() {
    const next = draft.trim()
    if (!next || next === name) {
      setEditing(false)
      return
    }
    try {
      await api.setDeviceName(deviceId, next)
      setEditing(false)
      setErr(null)
      onSaved()
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e))
    }
  }

  if (!editing) {
    return (
      <span className="device-name">
        {name ?? <span className="mono">{shortId(deviceId)}</span>}
        <button
          className="ghost rename"
          title="rename this device"
          onClick={() => {
            setDraft(name ?? '')
            setEditing(true)
          }}
        >
          ✎
        </button>
      </span>
    )
  }
  return (
    <span className="device-name editing">
      <input
        autoFocus
        value={draft}
        maxLength={64}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') save()
          if (e.key === 'Escape') setEditing(false)
        }}
      />
      <button className="ghost" onClick={save}>save</button>
      {err && <span className="error">{err}</span>}
    </span>
  )
}

function NetworkForm({ net, onSaved }: { net: NetConfig; onSaved: () => void }) {
  const [mode, setMode] = useState(net.relay_mode)
  const [urls, setUrls] = useState(net.relay_urls.join('\n'))
  const [lookup, setLookup] = useState(net.public_lookup)
  const [msg, setMsg] = useState<string | null>(null)
  const dirty =
    mode !== net.relay_mode ||
    lookup !== net.public_lookup ||
    urls.split('\n').map((u) => u.trim()).filter(Boolean).join('\n') !== net.relay_urls.join('\n')

  async function save() {
    setMsg(null)
    try {
      await api.setNetConfig({
        relay_mode: mode,
        relay_urls: mode === 'custom' ? urls.split('\n').map((u) => u.trim()).filter(Boolean) : [],
        public_lookup: lookup,
      })
      setMsg('saved — applies when this peer restarts')
      onSaved()
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e))
    }
  }

  return (
    <div className="net-form">
      <label className="net-row">
        <input type="radio" name="relay" checked={mode === 'default'} onChange={() => setMode('default')} />
        n0 public relays (default)
      </label>
      <label className="net-row">
        <input type="radio" name="relay" checked={mode === 'custom'} onChange={() => setMode('custom')} />
        custom relays
      </label>
      {mode === 'custom' && (
        <textarea
          rows={2}
          placeholder={'https://relay.example.com\none url per line'}
          value={urls}
          onChange={(e) => setUrls(e.target.value)}
        />
      )}
      <label className="net-row">
        <input type="radio" name="relay" checked={mode === 'disabled'} onChange={() => setMode('disabled')} />
        no relays — direct connections only
      </label>
      <label className="net-row lookup">
        <input type="checkbox" checked={lookup} onChange={(e) => setLookup(e.target.checked)} />
        publish this peer's address to the public lookup (DNS/pkarr)
      </label>
      <div className="net-actions">
        <button className="ghost" disabled={!dirty} onClick={save}>save network settings</button>
        {msg && <span className="hint">{msg}</span>}
      </div>
    </div>
  )
}

export function StatusView() {
  const [status, setStatus] = useState<Status | null>(null)
  const [error, setError] = useState(false)
  const [copied, setCopied] = useState(false)
  const [ticketIn, setTicketIn] = useState('')
  const [syncMsg, setSyncMsg] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [downloads, setDownloads] = useState<DownloadFile[]>([])

  const refresh = () => api.status().then(setStatus).catch(() => setError(true))

  useEffect(() => {
    refresh()
    api.downloads?.().then(setDownloads).catch(() => {})
  }, [])

  // endpoint_id → outcome of the latest probe round; 'probing' while in flight.
  const [pings, setPings] = useState<Record<string, PeerPing> | 'probing' | null>(null)

  async function pingPeers() {
    setPings('probing')
    try {
      const results = await api.pingPeers()
      setPings(Object.fromEntries(results.map((p) => [p.endpoint_id, p])))
      refresh() // a successful probe refreshes last-contact and transports
    } catch {
      setPings(null)
    }
  }

  async function syncNow() {
    if (!api.syncNow) return
    setBusy(true)
    setSyncMsg(null)
    try {
      const r = await api.syncNow(ticketIn.trim() || undefined)
      setSyncMsg(`sent ${r.sent ?? 0}, received ${r.received}, blobs ${r.blobs}`)
      refresh()
    } catch (e) {
      setSyncMsg(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  if (error) return <main className="stream"><p className="error">could not load status</p></main>
  if (!status) return <main className="stream" />

  const names = status.names ?? {}
  const rows = deviceRows(status)
  const selfName = names[status.device_id] ?? 'this device'
  const mapPeers: MapPeer[] = (status.peers ?? []).map((peer) => ({
    key: peer.endpoint_id,
    name: trunc(peer.device_id ? names[peer.device_id] ?? shortId(peer.device_id) : shortId(peer.endpoint_id), 18),
    sub:
      peer.discovery === 'ticket' ? 'via pairing ticket' :
      peer.discovery === 'inbound' ? 'it found us' :
      `seen ${agoLabel(peer.last_ok_ms)}`,
    peer,
  }))
  const health = status.health

  return (
    <main className="stream status">
      <h2>
        sync
        {health && (
          <span className={`health-dot ${health.color}`} title={
            health.color === 'green' ? 'every peer holds everything' :
            health.color === 'yellow' ? (
              (health.peers_behind ?? 0) > 0
                ? `${health.peers_behind} peer${health.peers_behind === 1 ? '' : 's'} behind (events or media)`
                : 'local entries not yet picked up by any peer'
            ) :
            'a peer has been out of touch for 48h+'
          } />
        )}
      </h2>

      {mapPeers.length > 0 && (
        <>
          <PeerMap self={trunc(selfName, 18)} selfSub="this device" peers={mapPeers} />
          <div className="pm-legend">
            <span className="lan">— LAN</span>
            <span className="relay">— public relay</span>
            <span className="internet">— internet</span>
            <span className="idle">⋯ idle</span>
            <span className="hint">arrows point the way the connection is opened</span>
          </div>
        </>
      )}

      <h3>
        devices
        {(status.peers ?? []).length > 0 && (
          <button className="ghost ping" onClick={pingPeers} disabled={pings === 'probing'}
            title="probe every known peer — reachable means able to sync right now">
            {pings === 'probing' ? 'pinging…' : '⇄ ping peers'}
          </button>
        )}
      </h3>
      <ul className="devices">
        {rows.map((row) => (
          <li key={row.key}>
            <span className="device-line">
              {row.isSelf ? '● ' : '○ '}
              {row.deviceId ? (
                <NameEditor deviceId={row.deviceId} name={row.name} onSaved={refresh} />
              ) : (
                <span className="mono">{shortId(row.key)}</span>
              )}
            </span>
            <span className="device-meta hint">
              {row.isSelf && (
                <span>
                  this device{row.headSeq !== undefined && ` · ${row.headSeq} events`}
                  {status.media && ` · media ${status.media.held} of ${status.media.referenced}`}
                  {status.media && status.media.evictable > 0 && ` (${status.media.evictable} let go by policy)`}
                </span>
              )}
              {!row.isSelf && row.peer && (
                <>
                  <span>{discoveryLabel(row.peer)}</span>
                  <span>{transportLabel(row.peer)}</span>
                  {pings && pings !== 'probing' && pings[row.peer.endpoint_id] && (
                    <PingLine ping={pings[row.peer.endpoint_id]} />
                  )}
                  {(() => {
                    const h = holdingsLabel(row.peer, status)
                    return <span className={h.behind ? 'behind' : undefined}>{h.text}</span>
                  })()}
                  {row.headSeq !== undefined && <span>{row.headSeq} events in the log</span>}
                </>
              )}
              {!row.isSelf && !row.peer && (
                <span>known from the log only{row.headSeq !== undefined && ` · ${row.headSeq} events`}</span>
              )}
            </span>
          </li>
        ))}
      </ul>

      <dl>
        {status.timeline && (
          <>
            <dt>journal</dt>
            <dd>
              {status.timeline.entries} entries ({status.trash} in trash)
              {status.timeline.first_recorded_at != null && status.timeline.last_recorded_at != null && (
                <span className="hint">
                  {' '}· {prettyDate(status.timeline.first_recorded_at)} → {prettyDate(status.timeline.last_recorded_at)}
                </span>
              )}
            </dd>
          </>
        )}
        {status.storage && (
          <>
            <dt>storage</dt>
            <dd>
              {prettySize(status.storage.blobs_bytes + status.storage.db_bytes)}
              <span className="hint">
                {' '}({prettySize(status.storage.blobs_bytes)} media store · {prettySize(status.storage.db_bytes)} database)
              </span>
            </dd>
          </>
        )}
        {status.net && (
          <>
            <dt>network</dt>
            <dd>
              <NetworkForm net={status.net} onSaved={refresh} />
            </dd>
          </>
        )}
        {status.ticket && (
          <>
            <dt>pair a device</dt>
            <dd>
              <button
                className="ghost"
                onClick={() => {
                  navigator.clipboard.writeText(status.ticket!)
                  setCopied(true)
                }}
              >
                {copied ? 'ticket copied' : 'copy pairing ticket'}
              </button>
            </dd>
          </>
        )}
        {api.syncNow && (
          <>
            <dt>sync now</dt>
            <dd className="sync-now">
              <input
                placeholder="peer ticket (blank = last used)"
                value={ticketIn}
                onChange={(e) => setTicketIn(e.target.value)}
              />
              <button className="ghost" disabled={busy} onClick={syncNow}>
                {busy ? 'syncing…' : 'sync'}
              </button>
              {syncMsg && <p className="hint">{syncMsg}</p>}
            </dd>
          </>
        )}
        {downloads.length > 0 && (
          <>
            <dt>get the apps</dt>
            <dd>
              <ul className="downloads">
                {downloads.map((f) => (
                  <li key={f.name}>
                    <a href={f.url} download={f.name}>
                      {prettyName(f.name)}
                    </a>{' '}
                    <span className="hint">{prettySize(f.size)}</span>
                  </li>
                ))}
              </ul>
            </dd>
          </>
        )}
      </dl>
      <p className="hint">
        a peer's status is only as fresh as our last contact with it.
      </p>
    </main>
  )
}
