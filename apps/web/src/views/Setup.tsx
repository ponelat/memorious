import { useState } from 'react'
import type { SetupApi } from '../api'

export function Setup({ setup, onDone }: { setup: SetupApi; onDone: () => void }) {
  const [ticket, setTicket] = useState('')
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')
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

  function startFresh() {
    if (password !== confirm) {
      setError("passwords don't match")
      return
    }
    run(() => setup.initFresh(password))
  }

  const havePassword = password !== ''

  return (
    <div className="login setup">
      <h1>memorious</h1>
      <p className="hint">this device has no journal yet</p>
      <input
        type="password"
        placeholder="master password (encrypts everything at rest)"
        value={password}
        onChange={(e) => setPassword(e.target.value)}
      />
      <input
        type="password"
        placeholder="repeat password (for a new journal)"
        value={confirm}
        onChange={(e) => setConfirm(e.target.value)}
      />
      <button disabled={busy || !havePassword} onClick={startFresh}>
        start a new journal
      </button>
      <p className="hint">— or join an existing one (same password as its other devices) —</p>
      <textarea
        placeholder="paste a pairing ticket from another device"
        value={ticket}
        rows={4}
        onChange={(e) => setTicket(e.target.value)}
      />
      <button
        disabled={busy || ticket.trim() === '' || !havePassword}
        onClick={() => run(() => setup.joinTicket(ticket.trim(), password))}
      >
        join
      </button>
      {error && <p className="error">{error}</p>}
    </div>
  )
}
