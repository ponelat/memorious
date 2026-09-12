import { useCallback, useEffect, useRef, useState } from 'react'
import { api, getToken } from './api'
import { Login } from './views/Login'
import { Setup } from './views/Setup'
import { Unlock } from './views/Unlock'
import { StreamView } from './views/StreamView'
import { TrashView } from './views/TrashView'
import { StatusView } from './views/StatusView'
import { Wordmark } from './components/Wordmark'
import { useAppearance } from './appearance'

export type View = 'stream' | 'trash' | 'status'

/** Marks the frame `scrolling` while the page moves and for a beat after it stops.
 *  The stream's timestamps and row rules only show in that state — at rest the
 *  page is just the entries. Class toggling on a ref, no re-render per scroll. */
function useScrollingClass(ref: React.RefObject<HTMLElement>) {
  useEffect(() => {
    let timer: number | undefined
    const onScroll = () => {
      ref.current?.classList.add('scrolling')
      window.clearTimeout(timer)
      timer = window.setTimeout(() => ref.current?.classList.remove('scrolling'), 900)
    }
    window.addEventListener('scroll', onScroll, { passive: true })
    return () => {
      window.removeEventListener('scroll', onScroll)
      window.clearTimeout(timer)
    }
  }, [ref])
}

export function App() {
  const [authed, setAuthed] = useState(() => !api.needsAuth || getToken() !== null)
  const frame = useRef<HTMLDivElement>(null)
  useScrollingClass(frame)
  useAppearance()
  const [setupState, setSetupState] = useState<'unknown' | 'ready' | 'empty' | 'locked'>(
    api.setup ? 'unknown' : 'ready',
  )
  const [view, setView] = useState<View>('stream')

  useEffect(() => {
    api.setup?.state().then(setSetupState)
  }, [])

  useEffect(() => {
    const onUnauthorized = () => setAuthed(false)
    // "reset this device" on the sync page: the journal is gone, back to first run.
    const onReset = () => {
      setView('stream')
      setSetupState('empty')
    }
    window.addEventListener('journal:unauthorized', onUnauthorized)
    window.addEventListener('journal:reset', onReset)
    return () => {
      window.removeEventListener('journal:unauthorized', onUnauthorized)
      window.removeEventListener('journal:reset', onReset)
    }
  }, [])

  const login = useCallback(async (passcode: string) => {
    const ok = await api.checkPasscode(passcode)
    if (ok) setAuthed(true)
    return ok
  }, [])

  if (setupState === 'unknown') return null
  if (setupState === 'empty' && api.setup) {
    return <Setup setup={api.setup} onDone={() => setSetupState('ready')} />
  }
  if (setupState === 'locked' && api.setup) {
    const { unlock } = api.setup
    return (
      <Unlock
        onUnlock={async (password) => {
          await unlock(password)
          setSetupState('ready')
        }}
      />
    )
  }
  if (!authed) return <Login onSubmit={login} />

  return (
    <div className="app" ref={frame}>
      <header className="topbar">
        <button
          className={view === 'stream' ? 'tab home active' : 'tab home'}
          onClick={() => setView('stream')}
          title="memorious"
        >
          <Wordmark className="small" />
        </button>
        <span className="spacer" />
        <button
          className={view === 'trash' ? 'tab active' : 'tab'}
          onClick={() => setView(view === 'trash' ? 'stream' : 'trash')}
        >
          trash
        </button>
        <button
          className={view === 'status' ? 'tab active' : 'tab'}
          onClick={() => setView(view === 'status' ? 'stream' : 'status')}
        >
          sync
        </button>
      </header>
      {view === 'stream' && <StreamView />}
      {view === 'trash' && <TrashView />}
      {view === 'status' && <StatusView />}
    </div>
  )
}
