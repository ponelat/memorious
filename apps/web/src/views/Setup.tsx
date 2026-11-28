import { useEffect, useState } from 'react'
import { api, type SetupApi } from '../api'
import type { JoinAsk } from '../api/types'
import { BrandHero } from '../components/BrandHero'

type Route = 'choose' | 'new' | 'join'

/** True on the browser/server build: the journal's master password is
 * already fixed (the operator's MEMORIOUS_PASSWORD), so the only secret
 * chosen here is the browser login passcode. False on desktop/Tauri, which
 * owns its core directly and picks the real master password right here. */
const web = api.needsAuth

/** First run: no journal on this device (desktop/phone) or an empty one
 * with no passcode yet (server). One choice — start fresh (desktop/phone:
 * choose a master password; server: pick a passcode for the journal
 * already sitting there) or join one from another device (pairing ticket
 * + the secret that unlocks it). Each choice is its own screen, as on the
 * phone. */
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
        <button onClick={() => setRoute('new')}>{web ? 'use this journal' : 'start a new journal'}</button>
        <button onClick={() => setRoute('join')}>join a peer</button>
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

/** Desktop/phone: choose the master password. It is the only key to the
 * journal, so the screen says so plainly and asks for an acknowledgement
 * before creating. Server: the master password is already fixed by the
 * operator's environment — this just picks the passcode that unlocks the
 * browser, so it skips the encryption warning and the written-it-down
 * ritual, neither of which is true here. */
function NewJournal({ setup, onDone, onBack }: { setup: SetupApi; onDone: () => void; onBack: () => void }) {
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')
  const [saved, setSaved] = useState(false)
  const { busy, error, run } = useRun(onDone)
  const canCreate = web
    ? !busy && password !== '' && password === confirm
    : !busy && password !== '' && password === confirm && saved
  const create = () => canCreate && run(() => setup.initFresh(password))

  return (
    <BrandHero
      headline={web ? 'Choose a\npasscode.' : 'Choose a\nmaster password.'}
      subline={
        web
          ? 'Unlocks this journal in a browser.'
          : 'It encrypts everything. There is no reset and no recovery. Lose it and the journal is gone.'
      }
    >
      <input
        type="password"
        placeholder={web ? 'passcode' : 'master password'}
        autoFocus
        value={password}
        onChange={(e) => setPassword(e.target.value)}
      />
      <input
        type="password"
        placeholder={web ? 'repeat passcode' : 'repeat password'}
        value={confirm}
        onChange={(e) => setConfirm(e.target.value)}
        onKeyDown={(e) => e.key === 'Enter' && create()}
      />
      {confirm !== '' && confirm !== password && <p className="error">{web ? "passcodes don't match" : "passwords don't match"}</p>}
      {!web && (
        <label className="hero-check">
          <input type="checkbox" checked={saved} onChange={(e) => setSaved(e.target.checked)} />
          I've written it down and kept it somewhere safe.
        </label>
      )}
      <button disabled={!canCreate} onClick={create}>
        {web ? 'open journal' : 'create journal'}
      </button>
      {error && <p className="error">{error}</p>}
      <button className="back" onClick={onBack} disabled={busy}>
        back
      </button>
    </BrandHero>
  )
}

/** Join a journal that already exists on another device. Desktop/phone type
 * in that journal's real master password (they own their core and need it
 * to decrypt). Server: the master password for the joined journal is
 * already sitting in this box's MEMORIOUS_PASSWORD — pasting the ticket is
 * the whole ask, and the second field just picks a fresh browser passcode. */
function JoinJournal({ setup, onDone, onBack }: { setup: SetupApi; onDone: () => void; onBack: () => void }) {
  const [ticket, setTicket] = useState('')
  const [secret, setSecret] = useState('')
  const [keep, setKeep] = useState<'everything' | 'visible'>('everything')
  const { busy, error, run } = useRun(onDone)
  const canJoin = !busy && ticket.trim() !== '' && secret !== ''
  const join = () => canJoin && run(() => setup.joinTicket(ticket.trim(), secret, keep))
  // while the join waits: what it waits on, the code to compare on the other peer, and until when it keeps asking
  const [ask, setAsk] = useState<JoinAsk | null>(null)
  useEffect(() => {
    if (!busy) { setAsk(null); return }
    const t = setInterval(() => void setup.joinProgress().then(setAsk).catch(() => {}), 1500)
    return () => clearInterval(t)
  }, [busy, setup])
  const until = ask?.until_ms ? new Date(ask.until_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : null
  const status = !ask
    ? null
    : ask.state === 'knocking'
      ? `Accept ${ask.code} on the other peer.${until ? ` Asking until ${until}.` : ''}`
      : `Can't reach it yet.${until ? ` Asking until ${until}.` : ''} Open it and accept ${ask.code}.`

  return (
    <BrandHero
      headline={'Join a peer.'}
      subline={web ? 'Paste an invite, then choose a passcode.' : "Paste an invite and that journal's master password."}
    >
      <textarea
        placeholder="invite"
        rows={3}
        autoFocus
        value={ticket}
        onChange={(e) => setTicket(e.target.value)}
      />
      <input
        type="password"
        placeholder={web ? 'passcode' : 'master password'}
        value={secret}
        onChange={(e) => setSecret(e.target.value)}
        onKeyDown={(e) => e.key === 'Enter' && join()}
      />
      <label className="row">
        <input type="radio" name="keep" checked={keep === 'everything'} onChange={() => setKeep('everything')} /> keep everything
      </label>
      <label className="row">
        <input type="radio" name="keep" checked={keep === 'visible'} onChange={() => setKeep('visible')} /> keep only what's visible
      </label>
      <button disabled={!canJoin} onClick={join}>
        {busy ? 'asking…' : 'join'}
      </button>
      {busy && status && <p>{status}</p>}
      {busy && setup.joinCancel && <button className="back" onClick={() => void setup.joinCancel?.()}>stop asking</button>}
      {error && <p className="error">{error}</p>}
      <button className="back" onClick={onBack} disabled={busy}>
        back
      </button>
    </BrandHero>
  )
}
