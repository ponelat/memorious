import { useEffect, useState } from 'react'
import { api, Entry } from '../api'
import { EntryRow } from '../components/EntryRow'

export function TrashView() {
  const [entries, setEntries] = useState<Entry[] | null>(null)

  useEffect(() => {
    api.trash().then(setEntries).catch(() => setEntries([]))
  }, [])

  if (entries === null) return <main className="stream" />
  return (
    <main className="stream">
      <h2>trash</h2>
      {entries.length === 0 && <p className="hint">empty</p>}
      <ol className="entries trashed">
        {entries.map((e) => (
          <li key={e.event_id}>
            <EntryRow entry={e} />
          </li>
        ))}
      </ol>
      <p className="hint">
        redacted entries stay here forever; their media is eventually garbage-collected.
      </p>
    </main>
  )
}
