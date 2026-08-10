import { useEffect, useState } from 'react'
import { Entry, mediaObjectUrl } from '../api'

function timeOf(ms: number): string {
  return new Date(ms).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', hour12: false })
}

export function useMediaUrl(entry: Entry | undefined): string | null {
  const [url, setUrl] = useState<string | null>(null)
  useEffect(() => {
    let live = true
    if (entry?.media) {
      mediaObjectUrl(entry.media).then((u) => live && setUrl(u))
    }
    return () => {
      live = false
    }
  }, [entry?.media?.hash])
  return url
}

export function EntryRow({
  entry,
  onRedact,
  onOpenPhoto,
}: {
  entry: Entry
  onRedact?: (e: Entry) => void
  onOpenPhoto?: () => void
}) {
  if (entry.kind === 'photo') return <PhotoSingle entry={entry} onOpen={onOpenPhoto} />
  if (entry.kind === 'audio') return <AudioRow entry={entry} onRedact={onRedact} />
  return (
    <div className="entry text">
      <span className="time">{timeOf(entry.recorded_at)}</span>
      <p>{entry.text}</p>
      {entry.annotation && <p className="annotation">{entry.annotation}</p>}
      {onRedact && (
        <button className="ghost redact" onClick={() => onRedact(entry)} title="move to trash">
          ×
        </button>
      )}
    </div>
  )
}

function PhotoSingle({ entry, onOpen }: { entry: Entry; onOpen?: () => void }) {
  const url = useMediaUrl(entry)
  return (
    <div className="entry photo">
      <span className="time">{timeOf(entry.recorded_at)}</span>
      <button className="polaroid" onClick={onOpen}>
        {url ? <img src={url} alt="" loading="lazy" /> : <span className="ph" />}
      </button>
      {entry.annotation && <p className="annotation">{entry.annotation}</p>}
    </div>
  )
}

/** Runs of consecutive photos collapse into a fanned stack (kept from v1). */
export function PhotoRun({ entries, onOpen }: { entries: Entry[]; onOpen: () => void }) {
  const shown = entries.slice(0, 3)
  return (
    <div className="entry photo">
      <span className="time">{timeOf(entries[entries.length - 1].recorded_at)}</span>
      <button className="fan" onClick={onOpen} title={`${entries.length} photos`}>
        {shown.map((e, i) => (
          <FanThumb key={e.event_id} entry={e} index={i} />
        ))}
        <span className="fan-count">{entries.length}</span>
      </button>
    </div>
  )
}

function FanThumb({ entry, index }: { entry: Entry; index: number }) {
  const url = useMediaUrl(entry)
  const rot = [-6, 3, -1][index % 3]
  return (
    <span className="polaroid fanned" style={{ transform: `rotate(${rot}deg)`, zIndex: 3 - index }}>
      {url ? <img src={url} alt="" loading="lazy" /> : <span className="ph" />}
    </span>
  )
}

export function AudioRow({
  entry,
  onRedact,
}: {
  entry: Entry
  onRedact?: (e: Entry) => void
}) {
  const url = useMediaUrl(entry)
  return (
    <div className="entry audio">
      <span className="time">{timeOf(entry.recorded_at)}</span>
      {url ? <audio controls preload="metadata" src={url} /> : <span className="ph audio-ph">audio…</span>}
      {entry.annotation && <p className="annotation">{entry.annotation}</p>}
      {onRedact && (
        <button className="ghost redact" onClick={() => onRedact(entry)} title="move to trash">
          ×
        </button>
      )}
    </div>
  )
}
