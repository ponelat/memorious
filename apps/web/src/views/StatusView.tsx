import { useEffect, useState } from 'react'
import { api, DownloadFile, Status } from '../api'

function prettySize(bytes: number): string {
  if (bytes > 1_000_000) return `${(bytes / 1_000_000).toFixed(1)} MB`
  if (bytes > 1_000) return `${Math.round(bytes / 1_000)} kB`
  return `${bytes} B`
}

/** "journal-desktop-macos-arm64.zip" → "journal desktop · macos arm64" */
function prettyName(name: string): string {
  return name.replace(/\.(zip|dmg|tar\.gz)$/, '').replace(/-/g, ' ')
}

export function StatusView() {
  const [status, setStatus] = useState<Status | null>(null)
  const [error, setError] = useState(false)
  const [copied, setCopied] = useState(false)
  const [ticketIn, setTicketIn] = useState('')
  const [syncMsg, setSyncMsg] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [downloads, setDownloads] = useState<DownloadFile[]>([])

  useEffect(() => {
    api.status().then(setStatus).catch(() => setError(true))
    api.downloads?.().then(setDownloads).catch(() => {})
  }, [])

  async function syncNow() {
    if (!api.syncNow) return
    setBusy(true)
    setSyncMsg(null)
    try {
      const r = await api.syncNow(ticketIn.trim() || undefined)
      setSyncMsg(`sent ${r.sent ?? 0}, received ${r.received}, blobs ${r.blobs}`)
      api.status().then(setStatus)
    } catch (e) {
      setSyncMsg(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  if (error) return <main className="stream"><p className="error">could not load status</p></main>
  if (!status) return <main className="stream" />

  return (
    <main className="stream status">
      <h2>sync</h2>
      <dl>
        <dt>this peer</dt>
        <dd className="mono">{status.device_id}</dd>
        <dt>entries</dt>
        <dd>
          {status.entries} ({status.trash} in trash)
        </dd>
        <dt>device heads</dt>
        <dd>
          <ul className="mono heads">
            {Object.entries(status.heads).map(([device, seq]) => (
              <li key={device}>
                {device === status.device_id ? '● ' : '○ '}
                {device.slice(0, 16)}… @ {seq}
              </li>
            ))}
          </ul>
        </dd>
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
