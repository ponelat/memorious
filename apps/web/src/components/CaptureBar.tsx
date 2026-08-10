import { FormEvent, useRef, useState } from 'react'
import { api, Entry } from '../api'

export function CaptureBar({ onCaptured }: { onCaptured: (e: Entry) => void }) {
  const [text, setText] = useState('')
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

  async function submitText(e: FormEvent) {
    e.preventDefault()
    if (text.trim() === '') return
    await guard(async () => {
      const entry = await api.captureText(text)
      setText('')
      onCaptured(entry)
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
      <form onSubmit={submitText} className="capture-text">
        <textarea
          placeholder="what's happening?"
          value={text}
          rows={text.includes('\n') ? 4 : 1}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              submitText(e)
            }
          }}
        />
        <button type="submit" disabled={busy || text.trim() === ''}>
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
