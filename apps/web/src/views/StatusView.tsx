import { useEffect, useState } from 'react'
import { api, Status } from '../api'

export function StatusView() {
  const [status, setStatus] = useState<Status | null>(null)
  const [error, setError] = useState(false)
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    api.status().then(setStatus).catch(() => setError(true))
  }, [])

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
      </dl>
      <p className="hint">
        a peer's status is only as fresh as our last contact with it.
      </p>
    </main>
  )
}
