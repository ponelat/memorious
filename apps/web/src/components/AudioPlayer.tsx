import { KeyboardEvent, PointerEvent, useEffect, useRef, useState } from 'react'

/** Only one entry plays at a time — starting one pauses whichever was going. */
let current: HTMLAudioElement | null = null

function fmt(seconds: number): string {
  if (!isFinite(seconds) || seconds < 0) return '–:––'
  const s = Math.floor(seconds)
  const m = Math.floor(s / 60)
  const h = Math.floor(m / 60)
  const mm = h ? String(m % 60).padStart(2, '0') : String(m)
  const ss = String(s % 60).padStart(2, '0')
  return h ? `${h}:${mm}:${ss}` : `${mm}:${ss}`
}

/**
 * The stream's audio player: an orange play/pause ring, elapsed and total time
 * in the small size, and a hairline you can scrub. Wraps a hidden <audio>; the
 * browser's own controls don't take the brand.
 */
export function AudioPlayer({ src, music }: { src: string; music?: boolean }) {
  const audio = useRef<HTMLAudioElement>(null)
  const bar = useRef<HTMLDivElement>(null)
  const [playing, setPlaying] = useState(false)
  const [time, setTime] = useState(0)
  const [duration, setDuration] = useState(NaN)
  const [failed, setFailed] = useState(false)
  const scrubbing = useRef(false)

  useEffect(() => {
    const el = audio.current
    if (!el) return
    const onTime = () => {
      if (!scrubbing.current) setTime(el.currentTime)
    }
    const onDuration = () => setDuration(el.duration)
    const onPlay = () => {
      if (current && current !== el) current.pause()
      current = el
      setPlaying(true)
    }
    const onPause = () => setPlaying(false)
    const onEnded = () => {
      setPlaying(false)
      setTime(0)
      el.currentTime = 0
    }
    const onError = () => setFailed(true)
    el.addEventListener('timeupdate', onTime)
    el.addEventListener('durationchange', onDuration)
    el.addEventListener('loadedmetadata', onDuration)
    el.addEventListener('play', onPlay)
    el.addEventListener('pause', onPause)
    el.addEventListener('ended', onEnded)
    el.addEventListener('error', onError)
    return () => {
      el.removeEventListener('timeupdate', onTime)
      el.removeEventListener('durationchange', onDuration)
      el.removeEventListener('loadedmetadata', onDuration)
      el.removeEventListener('play', onPlay)
      el.removeEventListener('pause', onPause)
      el.removeEventListener('ended', onEnded)
      el.removeEventListener('error', onError)
      if (current === el) current = null
    }
  }, [])

  function toggle() {
    const el = audio.current
    if (!el) return
    if (el.paused) void el.play().catch(() => setFailed(true))
    else el.pause()
  }

  function seekTo(clientX: number) {
    const el = audio.current
    const box = bar.current?.getBoundingClientRect()
    if (!el || !box || !isFinite(duration) || duration <= 0) return
    const ratio = Math.min(1, Math.max(0, (clientX - box.left) / box.width))
    const t = ratio * duration
    setTime(t)
    el.currentTime = t
  }

  function onPointerDown(e: PointerEvent<HTMLDivElement>) {
    scrubbing.current = true
    e.currentTarget.setPointerCapture(e.pointerId)
    seekTo(e.clientX)
  }

  function onPointerMove(e: PointerEvent<HTMLDivElement>) {
    if (scrubbing.current) seekTo(e.clientX)
  }

  function onPointerUp(e: PointerEvent<HTMLDivElement>) {
    if (!scrubbing.current) return
    scrubbing.current = false
    e.currentTarget.releasePointerCapture(e.pointerId)
  }

  function onKey(e: KeyboardEvent<HTMLDivElement>) {
    const el = audio.current
    if (!el || !isFinite(duration)) return
    const step = e.shiftKey ? 30 : 5
    if (e.key === 'ArrowRight') el.currentTime = Math.min(duration, el.currentTime + step)
    else if (e.key === 'ArrowLeft') el.currentTime = Math.max(0, el.currentTime - step)
    else if (e.key === ' ' || e.key === 'Enter') toggle()
    else return
    e.preventDefault()
    setTime(el.currentTime)
  }

  const ratio = isFinite(duration) && duration > 0 ? Math.min(1, time / duration) : 0

  return (
    <div className={`player${playing ? ' playing' : ''}${failed ? ' failed' : ''}`}>
      <audio ref={audio} src={src} preload="metadata" />
      {music && (
        <span className="music-mark" title="music recording">
          ♪
        </span>
      )}
      <button
        type="button"
        className="pp"
        onClick={toggle}
        disabled={failed}
        aria-label={playing ? 'pause' : 'play'}
        title={playing ? 'pause' : 'play'}
      >
        {playing ? (
          <svg viewBox="0 0 12 12" aria-hidden="true">
            <rect x="2" y="1.5" width="3" height="9" />
            <rect x="7" y="1.5" width="3" height="9" />
          </svg>
        ) : (
          <svg viewBox="0 0 12 12" aria-hidden="true">
            <path d="M3 1.5 L10.5 6 L3 10.5 Z" />
          </svg>
        )}
      </button>
      {failed ? (
        <span className="t">audio unavailable</span>
      ) : (
        <>
          <span className="t elapsed">{fmt(time)}</span>
          <div
            ref={bar}
            className="bar"
            role="slider"
            tabIndex={0}
            aria-label="position"
            aria-valuemin={0}
            aria-valuemax={isFinite(duration) ? Math.round(duration) : 0}
            aria-valuenow={Math.round(time)}
            aria-valuetext={`${fmt(time)} of ${fmt(duration)}`}
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onPointerCancel={onPointerUp}
            onKeyDown={onKey}
          >
            <span className="track">
              <span className="fill" style={{ width: `${ratio * 100}%` }} />
              <span className="knob" style={{ left: `${ratio * 100}%` }} />
            </span>
          </div>
          <span className="t total">{fmt(duration)}</span>
        </>
      )}
    </div>
  )
}
