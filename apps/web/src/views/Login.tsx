import { FormEvent, useState } from 'react'
import { BrandHero } from '../components/BrandHero'

/** Browser passcode for the server peer: the same opening screen as the
 * desktop and the phone, with the one field this host needs. */
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
    <BrandHero headline={'One place\nto capture.'} subline="Enter the passcode for this journal.">
      <form className="hero-form" onSubmit={submit}>
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
    </BrandHero>
  )
}
