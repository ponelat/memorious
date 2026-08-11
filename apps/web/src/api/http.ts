import type { Entry, FeedPage, JournalApi, MediaRef, Status } from './types'

const TOKEN_KEY = 'memorious.passcode'

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY)
}

export function setToken(token: string | null) {
  if (token === null) localStorage.removeItem(TOKEN_KEY)
  else localStorage.setItem(TOKEN_KEY, token)
}

class HttpError extends Error {
  constructor(public status: number, message: string) {
    super(message)
  }
}

async function request(path: string, init?: RequestInit): Promise<Response> {
  const headers = new Headers(init?.headers)
  const token = getToken()
  if (token) headers.set('Authorization', `Bearer ${token}`)
  const resp = await fetch(path, { ...init, headers })
  if (resp.status === 401) {
    setToken(null)
    window.dispatchEvent(new Event('journal:unauthorized'))
    throw new HttpError(401, 'unauthorized')
  }
  if (!resp.ok) {
    let msg = `${resp.status}`
    try {
      msg = (await resp.json()).error ?? msg
    } catch {}
    throw new HttpError(resp.status, msg)
  }
  return resp
}

async function json<T>(path: string, init?: RequestInit): Promise<T> {
  return (await request(path, init)).json()
}

function upload(path: string, file: Blob): Promise<Entry> {
  const form = new FormData()
  form.append('file', file)
  return json<Entry>(path, { method: 'POST', body: form })
}

export const httpApi: JournalApi = {
  needsAuth: true,

  async checkPasscode(passcode: string) {
    const resp = await fetch('/api/auth/check', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ passcode }),
    })
    if (resp.status === 204) {
      setToken(passcode)
      return true
    }
    return false
  },

  captureText(text: string) {
    return json<Entry>('/api/capture/text', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text }),
    })
  },

  capturePhoto: (file) => upload('/api/capture/photo', file),
  captureAudio: (file) => upload('/api/capture/audio', file),

  feed(before?: number) {
    const q = before ? `?before=${before}` : ''
    return json<FeedPage>(`/api/feed${q}`)
  },

  async mediaBlob(media: MediaRef) {
    return (await request(media.url)).blob()
  },

  async redact(eventId: string) {
    await request('/api/redact', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ event_id: eventId }),
    })
  },

  async trash() {
    return (await json<{ entries: Entry[] }>('/api/trash')).entries
  },

  async search(q: string) {
    return (await json<{ entries: Entry[] }>(`/api/search?q=${encodeURIComponent(q)}`)).entries
  },

  status() {
    return json<Status>('/api/status')
  },

  async downloads() {
    return (await json<{ files: import('./types').DownloadFile[] }>('/api/downloads')).files
  },
}

