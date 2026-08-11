import { useState } from 'react'

/** Master-password prompt for hosts whose journal is encrypted at rest
 *  (desktop). Shown when the keychain has no cached password this launch. */
export function Unlock({ onUnlock }: { onUnlock: (password: string) => Promise<void> }) {
  const [password, setPassword] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function submit() {
    if (password === '' || busy) return
    setBusy(true)
    setError(null)
    try {
      await onUnlock(password)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      setBusy(false)
    }
  }

  return (
    <div className="login setup">
      <h1>memorious</h1>
      <p className="hint">this journal is encrypted</p>
      <input
        type="password"
        placeholder="master password"
        value={password}
        autoFocus
        onChange={(e) => setPassword(e.target.value)}
        onKeyDown={(e) => e.key === 'Enter' && submit()}
      />
      <button disabled={busy || password === ''} onClick={submit}>
        unlock
      </button>
      {error && <p className="error">{error}</p>}
    </div>
  )
}
