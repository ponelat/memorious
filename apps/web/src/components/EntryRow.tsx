import { useEffect, useRef, useState } from 'react'
import { Entry, mediaObjectUrl } from '../api'
import { AudioPlayer } from './AudioPlayer'
import { linkify } from './Linkify'
import { Marked } from './SearchMatch'

function timeOf(ms: number, dated = false): string {
  const t = new Date(ms).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', hour12: false })
  // Search results have no day headers, so the date rides with the time.
  return dated ? `${new Date(ms).toLocaleDateString(undefined, { day: 'numeric', month: 'short' })} ${t}` : t
}

/** The copyable text of an entry: its own text, else its transcript/OCR. */
function copyableOf(entry: Entry): string | null {
  if (entry.text) return entry.text
  if (entry.annotation) return entry.annotation
  return null
}

/** Clipboard write with a fallback for webviews without navigator.clipboard
 * (the Tauri shell's WKWebView, older mobile browsers). */
function copyText(text: string) {
  if (navigator.clipboard?.writeText) {
    navigator.clipboard.writeText(text)
    return
  }
  const ta = document.createElement('textarea')
  ta.value = text
  document.body.appendChild(ta)
  ta.select()
  document.execCommand('copy')
  ta.remove()
}

/** Subtle per-entry copy control (ChatGPT-thread style): fades in on row
 * hover, always faintly visible on touch screens, flashes a ✓ once copied. */
function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false)
  return (
    <button
      className={copied ? 'ghost copy copied' : 'ghost copy'}
      title="copy text"
      onClick={() => {
        copyText(text)
        setCopied(true)
        setTimeout(() => setCopied(false), 1500)
      }}
    >
      {copied ? '✓' : '⧉'}
    </button>
  )
}

/** Text capped at 7 lines with a more/less toggle — used for entry text and
 * transcripts alike. The toggle only appears when the clamp hides lines. */
function ClampedText({ text, className }: { text: string; className?: string }) {
  const [expanded, setExpanded] = useState(false)
  const [overflows, setOverflows] = useState(false)
  const ref = useRef<HTMLParagraphElement>(null)
  useEffect(() => {
    const el = ref.current
    if (!el) return
    const check = () => setOverflows(el.scrollHeight > el.clientHeight + 1)
    check()
    const ro = new ResizeObserver(check)
    ro.observe(el)
    return () => ro.disconnect()
  }, [text])
  return (
    <div className={className ? `clampable ${className}` : 'clampable'}>
      <p ref={ref} className={expanded ? undefined : 'clamped'}>
        {linkify(text, (t) => <Marked text={t} />)}
      </p>
      {(overflows || expanded) && (
        <button className="ghost more" onClick={() => setExpanded(!expanded)}>
          {expanded ? 'less' : 'more'}
        </button>
      )}
    </div>
  )
}

/** Transcript/OCR text: the clamp plus the annotation left-rule styling. */
function Annotation({ text }: { text: string }) {
  return <ClampedText text={text} className="annotation" />
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
  dated,
  onRedact,
  onOpenPhoto,
}: {
  entry: Entry
  dated?: boolean
  onRedact?: (e: Entry) => void
  onOpenPhoto?: () => void
}) {
  if (entry.kind === 'photo') return <PhotoSingle entry={entry} dated={dated} onOpen={onOpenPhoto} />
  if (entry.kind === 'video') return <VideoSingle entry={entry} dated={dated} onOpen={onOpenPhoto} />
  if (entry.kind === 'audio') return <AudioRow entry={entry} dated={dated} onRedact={onRedact} />
  return (
    <div className="entry text">
      <span className="time">{timeOf(entry.recorded_at, dated)}</span>
      <ClampedText text={entry.text ?? ''} />
      <span className="actions">
        {copyableOf(entry) && <CopyButton text={copyableOf(entry)!} />}
        {onRedact && (
          <button className="ghost redact" onClick={() => onRedact(entry)} title="move to trash">
            ×
          </button>
        )}
      </span>
    </div>
  )
}

function PhotoSingle({ entry, dated, onOpen }: { entry: Entry; dated?: boolean; onOpen?: () => void }) {
  const url = useMediaUrl(entry)
  return (
    <div className="entry photo">
      <span className="time">{timeOf(entry.recorded_at, dated)}</span>
      <button className="polaroid" onClick={onOpen}>
        {url ? <img src={url} alt="" loading="lazy" /> : <span className="ph" />}
      </button>
      {entry.annotation && <Annotation text={entry.annotation} />}
      {copyableOf(entry) && (
        <span className="actions">
          <CopyButton text={copyableOf(entry)!} />
        </span>
      )}
    </div>
  )
}

function VideoSingle({ entry, dated, onOpen }: { entry: Entry; dated?: boolean; onOpen?: () => void }) {
  const url = useMediaUrl(entry)
  return (
    <div className="entry photo">
      <span className="time">{timeOf(entry.recorded_at, dated)}</span>
      <button className="polaroid video" onClick={onOpen}>
        {url ? (
          <>
            <video src={url} muted playsInline preload="metadata" />
            <span className="play-badge">▶</span>
          </>
        ) : (
          <span className="ph" />
        )}
      </button>
      {entry.annotation && <Annotation text={entry.annotation} />}
      {copyableOf(entry) && (
        <span className="actions">
          <CopyButton text={copyableOf(entry)!} />
        </span>
      )}
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
  dated,
  onRedact,
}: {
  entry: Entry
  dated?: boolean
  onRedact?: (e: Entry) => void
}) {
  const music = entry.audio_kind === 'music'
  const evicted = entry.media?.evicted === true
  const url = useMediaUrl(evicted ? undefined : entry)
  return (
    <div className={`entry audio${music ? ' music' : ''}`}>
      <span className="time">{timeOf(entry.recorded_at, dated)}</span>
      {music && !url && <span className="music-mark" title="music recording">♪</span>}
      {evicted ? (
        <span className="ph audio-ph" title="audio pruned from this device; the transcript is the record">
          audio pruned
        </span>
      ) : url ? (
        <AudioPlayer src={url} music={music} />
      ) : (
        <span className="ph audio-ph">audio…</span>
      )}
      {!music && entry.annotation && <Annotation text={entry.annotation} />}
      <span className="actions">
        {copyableOf(entry) && <CopyButton text={copyableOf(entry)!} />}
        {onRedact && (
          <button className="ghost redact" onClick={() => onRedact(entry)} title="move to trash">
            ×
          </button>
        )}
      </span>
    </div>
  )
}
