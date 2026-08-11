import { useState } from 'react'
import type { SetupApi } from '../api'

export function Setup({ setup, onDone }: { setup: SetupApi; onDone: () => void }) {
  const [ticket, setTicket] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function run(work: () => Promise<unknown>) {
    setBusy(true)
    setError(null)
    try {
      await work()
      onDone()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="login setup">
      <h1>memorious</h1>
      <p className="hint">this device has no journal yet</p>
      <button disabled={busy} onClick={() => run(() => setup.initFresh())}>
        start a new journal
      </button>
      <p className="hint">— or join an existing one —</p>
      <textarea
        placeholder="paste a pairing ticket from another device"
        value={ticket}
        rows={4}
        onChange={(e) => setTicket(e.target.value)}
      />
      <button
        disabled={busy || ticket.trim() === ''}
        onClick={() => run(() => setup.joinTicket(ticket.trim()))}
      >
        join
      </button>
      {error && <p className="error">{error}</p>}
    </div>
  )
}
