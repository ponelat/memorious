import { useCallback, useEffect, useState } from 'react'
import { api, getToken } from './api'
import { Login } from './views/Login'
import { StreamView } from './views/StreamView'
import { TrashView } from './views/TrashView'
import { StatusView } from './views/StatusView'

export type View = 'stream' | 'trash' | 'status'

export function App() {
  const [authed, setAuthed] = useState(() => getToken() !== null)
  const [view, setView] = useState<View>('stream')

  useEffect(() => {
    const onUnauthorized = () => setAuthed(false)
    window.addEventListener('journal:unauthorized', onUnauthorized)
    return () => window.removeEventListener('journal:unauthorized', onUnauthorized)
  }, [])

  const login = useCallback(async (passcode: string) => {
    const ok = await api.checkPasscode(passcode)
    if (ok) setAuthed(true)
    return ok
  }, [])

  if (!authed) return <Login onSubmit={login} />

  return (
    <div className="app">
      <header className="topbar">
        <button
          className={view === 'stream' ? 'tab active' : 'tab'}
          onClick={() => setView('stream')}
        >
          journal
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
