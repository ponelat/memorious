import { FormEvent, useState } from 'react'

export function Login({ onSubmit }: { onSubmit: (passcode: string) => Promise<boolean> }) {
  const [passcode, setPasscode] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function submit(e: FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      const ok = await onSubmit(passcode)
      if (!ok) setError('wrong passcode')
    } catch {
      setError('could not reach the journal')
    } finally {
      setBusy(false)
    }
  }

  return (
    <form className="login" onSubmit={submit}>
      <h1>journal</h1>
      <input
        type="password"
        inputMode="numeric"
        autoFocus
        placeholder="passcode"
        value={passcode}
        onChange={(e) => setPasscode(e.target.value)}
      />
      <button disabled={busy || passcode.length === 0}>enter</button>
      {error && <p className="error">{error}</p>}
    </form>
  )
}
