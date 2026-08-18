import { ClipboardEvent, FormEvent, useRef, useState } from 'react'
import { api, Entry } from '../api'

/** Media staged in the capture bar (pasted) before submit, ChatGPT-style. */
interface Pending {
  id: string
  file: Blob
  kind: 'photo' | 'video' | 'audio'
  /** Object URL for the thumbnail; revoked when the item leaves the tray. */
  url: string
  name: string
}

let pendingSeq = 0

/**
 * Files on the clipboard. `files` is what most engines fill in, but some only
 * expose the image through `items` (and WebKitGTK before 2.50.2 exposes neither —
 * that one is fixed by the webkitgtk the Nix build pins, not by anything here).
 */
function pastedFiles(data: DataTransfer | null): File[] {
  const files = Array.from(data?.files ?? [])
  if (files.length > 0) return files
  return Array.from(data?.items ?? [])
    .filter((i) => i.kind === 'file')
    .map((i) => i.getAsFile())
    .filter((f): f is File => f !== null)
}

function pendingKind(type: string): Pending['kind'] | null {
  if (type.startsWith('image/')) return 'photo'
  if (type.startsWith('video/')) return 'video'
  if (type.startsWith('audio/')) return 'audio'
  return null
}

export function CaptureBar({ onCaptured }: { onCaptured: (e: Entry) => void }) {
  const [text, setText] = useState('')
  const [pending, setPending] = useState<Pending[]>([])
  const [busy, setBusy] = useState(false)
  const [recording, setRecording] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const fileInput = useRef<HTMLInputElement>(null)
  const recorder = useRef<MediaRecorder | null>(null)
  const chunks = useRef<Blob[]>([])

  async function guard<T>(work: () => Promise<T>) {
    setBusy(true)
    setError(null)
    try {
      return await work()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'failed')
    } finally {
      setBusy(false)
    }
  }

  function stageFiles(files: File[]): boolean {
    const staged: Pending[] = []
    for (const file of files) {
      const kind = pendingKind(file.type)
      if (!kind) continue
      staged.push({
        id: `p${pendingSeq++}`,
        file,
        kind,
        url: URL.createObjectURL(file),
        name: file.name || kind,
      })
    }
    if (staged.length > 0) {
      setPending((cur) => [...cur, ...staged])
      setError(null)
    }
    return staged.length > 0
  }

  function onPaste(e: ClipboardEvent<HTMLTextAreaElement>) {
    const files = pastedFiles(e.clipboardData)
    if (files.length === 0) return // plain text: let it land in the input
    // Media paste: keep any filename text out of the input.
    e.preventDefault()
    if (!stageFiles(files)) setError('only photos, videos, and audio can be attached')
  }

  function unstage(item: Pending) {
    URL.revokeObjectURL(item.url)
    setPending((cur) => cur.filter((p) => p.id !== item.id))
  }

  async function submit(e: FormEvent) {
    e.preventDefault()
    if (busy || (text.trim() === '' && pending.length === 0)) return
    await guard(async () => {
      // Attachments in the order they were staged, then the text.
      for (const item of pending) {
        const capture =
          item.kind === 'photo'
            ? api.capturePhoto
            : item.kind === 'video'
              ? api.captureVideo
              : api.captureAudio
        try {
          onCaptured(await capture(item.file))
        } catch (err) {
          // Leave this item (and the rest) staged so nothing is lost.
          throw err instanceof Error ? new Error(`${item.name}: ${err.message}`) : err
        }
        unstage(item)
      }
      if (text.trim() !== '') {
        const entry = await api.captureText(text)
        setText('')
        onCaptured(entry)
      }
    })
  }

  async function onPhotoPicked(files: FileList | null) {
    if (!files) return
    for (const file of Array.from(files)) {
      await guard(async () => onCaptured(await api.capturePhoto(file)))
    }
    if (fileInput.current) fileInput.current.value = ''
  }

  async function toggleRecording() {
    if (recording) {
      recorder.current?.stop()
      return
    }
    setError(null)
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true })
      // Safari records audio/mp4 (AAC) natively; Chrome falls back to webm and
      // the server transcodes.
      const mime = MediaRecorder.isTypeSupported('audio/mp4')
        ? 'audio/mp4'
        : 'audio/webm'
      const rec = new MediaRecorder(stream, { mimeType: mime })
      chunks.current = []
      rec.ondataavailable = (e) => chunks.current.push(e.data)
      rec.onstop = async () => {
        stream.getTracks().forEach((t) => t.stop())
        setRecording(false)
        const blob = new Blob(chunks.current, { type: mime })
        if (blob.size > 0) {
          await guard(async () => onCaptured(await api.captureAudio(blob)))
        }
      }
      rec.start()
      recorder.current = rec
      setRecording(true)
    } catch {
      setError('microphone unavailable')
    }
  }

  return (
    <div className="capture">
      {pending.length > 0 && (
        <div className="capture-attachments">
          {pending.map((item) => (
            <span key={item.id} className={`attach ${item.kind}`}>
              {item.kind === 'photo' && <img src={item.url} alt={item.name} />}
              {item.kind === 'video' && (
                <>
                  <video src={item.url} muted playsInline preload="metadata" />
                  <span className="play-badge">▶</span>
                </>
              )}
              {item.kind === 'audio' && <span className="attach-audio">♪ {item.name}</span>}
              <button
                type="button"
                className="rm"
                onClick={() => unstage(item)}
                title="remove"
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}
      <form onSubmit={submit} className="capture-text">
        <textarea
          placeholder="what's happening?"
          value={text}
          rows={text.includes('\n') ? 4 : 1}
          onChange={(e) => setText(e.target.value)}
          onPaste={onPaste}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              submit(e)
            }
          }}
        />
        <button type="submit" disabled={busy || (text.trim() === '' && pending.length === 0)}>
          add
        </button>
      </form>
      <div className="capture-media">
        <button type="button" onClick={() => fileInput.current?.click()} disabled={busy}>
          photo
        </button>
        <input
          ref={fileInput}
          type="file"
          accept="image/*"
          multiple
          capture="environment"
          hidden
          onChange={(e) => onPhotoPicked(e.target.files)}
        />
        <button
          type="button"
          className={recording ? 'recording' : ''}
          onClick={toggleRecording}
          disabled={busy && !recording}
        >
          {recording ? '■ stop' : '● rec'}
        </button>
      </div>
      {error && <p className="error">{error}</p>}
    </div>
  )
}
