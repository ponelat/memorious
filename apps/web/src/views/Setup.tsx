import { useState } from 'react'
import type { SetupApi } from '../api'
import { BrandHero } from '../components/BrandHero'

type Route = 'choose' | 'new' | 'join'

/** First run: no journal on this device. One choice — start a new journal
 * (choose a master password) or join one from another device (pairing
 * ticket + that journal's password). Each choice is its own screen, as on
 * the phone. */
export function Setup({ setup, onDone }: { setup: SetupApi; onDone: () => void }) {
  const [route, setRoute] = useState<Route>('choose')

  if (route === 'new') return <NewJournal setup={setup} onDone={onDone} onBack={() => setRoute('choose')} />
  if (route === 'join') return <JoinJournal setup={setup} onDone={onDone} onBack={() => setRoute('choose')} />
  return (
    <BrandHero
      headline={'One place\nto capture.'}
      subline={'Capture now. Sort later.\nNo account. Your journal lives on your devices.'}
    >
      <div className="hero-stack">
        <button onClick={() => setRoute('new')}>start a new journal</button>
        <button onClick={() => setRoute('join')}>join from another device</button>
      </div>
    </BrandHero>
  )
}

function useRun(onDone: () => void) {
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
  return { busy, error, run }
}

/** Choose the master password. It is the only key to the journal, so the
 * screen says so plainly and asks for an acknowledgement before creating. */
function NewJournal({ setup, onDone, onBack }: { setup: SetupApi; onDone: () => void; onBack: () => void }) {
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')
  const [saved, setSaved] = useState(false)
  const { busy, error, run } = useRun(onDone)
  const canCreate = !busy && password !== '' && password === confirm && saved
  const create = () => canCreate && run(() => setup.initFresh(password))

  return (
    <BrandHero
      headline={'Choose a\nmaster password.'}
      subline="It encrypts everything. There is no reset and no recovery. Lose it and the journal is gone."
    >
      <input
        type="password"
        placeholder="master password"
        autoFocus
        value={password}
        onChange={(e) => setPassword(e.target.value)}
      />
      <input
        type="password"
        placeholder="repeat password"
        value={confirm}
        onChange={(e) => setConfirm(e.target.value)}
        onKeyDown={(e) => e.key === 'Enter' && create()}
      />
      {confirm !== '' && confirm !== password && <p className="error">passwords don't match</p>}
      <label className="hero-check">
        <input type="checkbox" checked={saved} onChange={(e) => setSaved(e.target.checked)} />
        I've written it down and kept it somewhere safe.
      </label>
      <button disabled={!canCreate} onClick={create}>
        create journal
      </button>
      {error && <p className="error">{error}</p>}
      <button className="back" onClick={onBack} disabled={busy}>
        back
      </button>
    </BrandHero>
  )
}

/** Join a journal that already exists on another device. */
function JoinJournal({ setup, onDone, onBack }: { setup: SetupApi; onDone: () => void; onBack: () => void }) {
  const [ticket, setTicket] = useState('')
  const [password, setPassword] = useState('')
  const { busy, error, run } = useRun(onDone)
  const canJoin = !busy && ticket.trim() !== '' && password !== ''
  const join = () => canJoin && run(() => setup.joinTicket(ticket.trim(), password))

  return (
    <BrandHero
      headline={'Join from\nanother device.'}
      subline="On the other device, open sync and copy its pairing ticket. Use that journal's master password."
    >
      <textarea
        placeholder="pairing ticket"
        rows={3}
        autoFocus
        value={ticket}
        onChange={(e) => setTicket(e.target.value)}
      />
      <input
        type="password"
        placeholder="master password"
        value={password}
        onChange={(e) => setPassword(e.target.value)}
        onKeyDown={(e) => e.key === 'Enter' && join()}
      />
      <button disabled={!canJoin} onClick={join}>
        {busy ? 'joining…' : 'join'}
      </button>
      {error && <p className="error">{error}</p>}
      <button className="back" onClick={onBack} disabled={busy}>
        back
      </button>
    </BrandHero>
  )
}
