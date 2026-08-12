import { useEffect, useState } from 'react'
import { Entry } from '../api'
import { useMediaUrl } from './EntryRow'

export function Lightbox({
  entries,
  onClose,
  onRedact,
}: {
  entries: Entry[]
  onClose: () => void
  onRedact: (e: Entry) => void
}) {
  const [index, setIndex] = useState(0)
  const entry = entries[index]
  const url = useMediaUrl(entry)

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
      if (e.key === 'ArrowRight') setIndex((i) => Math.min(i + 1, entries.length - 1))
      if (e.key === 'ArrowLeft') setIndex((i) => Math.max(i - 1, 0))
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [entries.length, onClose])

  return (
    <div className="lightbox" onClick={onClose}>
      <div className="lightbox-inner" onClick={(e) => e.stopPropagation()}>
        {url &&
          (entry.kind === 'video' ? (
            <video src={url} controls autoPlay playsInline />
          ) : (
            <img src={url} alt="" />
          ))}
        <div className="lightbox-bar">
          {entries.length > 1 && (
            <span>
              {index + 1} / {entries.length}
            </span>
          )}
          <span>{new Date(entry.recorded_at).toLocaleString()}</span>
          <button className="ghost" onClick={() => onRedact(entry)}>
            trash
          </button>
          <button className="ghost" onClick={onClose}>
            close
          </button>
        </div>
        {entries.length > 1 && (
          <>
            <button
              className="nav prev"
              disabled={index === 0}
              onClick={() => setIndex((i) => i - 1)}
            >
              ‹
            </button>
            <button
              className="nav next"
              disabled={index === entries.length - 1}
              onClick={() => setIndex((i) => i + 1)}
            >
              ›
            </button>
          </>
        )}
      </div>
    </div>
  )
}
