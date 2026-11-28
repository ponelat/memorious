import type { AgentKey, ArchiveExport, ArchiveOptions, Entry, FeedPage, InkRecord, JournalApi, MediaRef, NetConfig, Status, SyncReport } from './types'

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

/** Save a response body to disk: streamed straight to a file where the browser allows it (no copy in memory), else as a blob. */
async function saveResponse(resp: Response, filename: string): Promise<void> {
  const w = window as unknown as { showSaveFilePicker?: (o: unknown) => Promise<{ createWritable(): Promise<WritableStream> }> }
  if (w.showSaveFilePicker && resp.body) {
    try {
      const handle = await w.showSaveFilePicker({ suggestedName: filename, types: [{ description: 'zip', accept: { 'application/zip': ['.zip'] } }] })
      await resp.body.pipeTo(await handle.createWritable())
      return
    } catch (e) {
      if (e instanceof DOMException && e.name === 'AbortError') throw e // the person cancelled the dialog
      // fall through to the blob route
    }
  }
  const url = URL.createObjectURL(await resp.blob())
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  document.body.appendChild(a)
  a.click()
  a.remove()
  setTimeout(() => URL.revokeObjectURL(url), 60_000)
}

/** Poll the server's join job until it is done; a failure throws with the server's reason. */
async function waitForJoin(): Promise<void> {
  for (;;) {
    await new Promise((r) => setTimeout(r, 1500))
    let job: { state: string; error?: string } | undefined
    try {
      job = (await json<{ job: { state: string; error?: string } }>('/api/setup/join-progress')).job
    } catch {
      continue // a blip while the server swaps journals; keep polling
    }
    if (job?.state === 'done') return
    if (job?.state === 'failed') throw new Error(job.error ?? 'join failed')
    if (job?.state === 'idle') throw new Error('join stopped')
  }
}

export const httpApi: JournalApi = {
  getRetention: () => json('/api/retention'),
  setRetention: (preset) => json('/api/retention', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ preset }) }),
  async setIsolated(on: boolean) {
    await request('/api/isolate', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ isolate: on }) })
  },
  async createArchiveLink(opts) {
    const r = await json<{ export: ArchiveExport }>('/api/exports', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ format: opts.format, media: opts.media, ttl_hours: opts.ttlHours }),
    })
    return r.export
  },
  async listArchiveLinks() {
    return (await json<{ exports: ArchiveExport[] }>('/api/exports')).exports
  },
  async deleteArchiveLink(id) {
    await request(`/api/exports/${id}`, { method: 'DELETE' })
  },
  async exportArchive(opts: ArchiveOptions) {
    const resp = await request(`/api/export?format=${opts.format}&media=${opts.media ? 1 : 0}`)
    const cd = resp.headers.get('content-disposition') ?? ''
    const name = /filename="([^"]+)"/.exec(cd)?.[1] ?? 'memorious-archive.zip'
    await saveResponse(resp, name)
    return name
  },
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
  captureAudio: (file, kind = 'voice') => upload(`/api/capture/audio?kind=${kind}`, file),
  captureVideo: (file) => upload('/api/capture/video', file),

  async pingPeers() {
    const r = await json<{ pings: import('./types').PeerPing[] }>('/api/peers/ping', { method: 'POST' })
    return r.pings
  },

  peersHistory() {
    return json<import('./types').PeersHistory>('/api/peers/history')
  },

  async acceptKnock(endpointId: string) {
    await json('/api/knocks/accept', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ endpoint_id: endpointId }) })
  },
  async rejectKnock(endpointId: string) {
    await json('/api/knocks/reject', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ endpoint_id: endpointId }) })
  },
  async removePeer(endpointId: string) {
    await json('/api/peers/remove', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ endpoint_id: endpointId }) })
  },

  async setPeerRequired(endpointId: string, required: boolean) {
    await request('/api/peers/required', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ endpoint_id: endpointId, required }),
    })
  },

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

  async related(q: string) {
    const r = await json<{ related: { word: string }[] }>(`/api/search/related?q=${encodeURIComponent(q)}`)
    return r.related.map((x) => x.word)
  },
  async search(q: string) {
    return (await json<{ entries: Entry[] }>(`/api/search?q=${encodeURIComponent(q)}`)).entries
  },

  status() {
    return json<Status>('/api/status')
  },

  async setDeviceName(deviceId: string, name: string) {
    await request('/api/device-name', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ device_id: deviceId, name }),
    })
  },

  ink: {
    async months() {
      return (await json<{ months: string[] }>('/api/ink')).months
    },
    list(month: string, after?: number) {
      return json<{ device_id: string; cursor: number; events: InkRecord[] }>(`/api/ink/${month}${after ? `?after=${after}` : ''}`)
    },
    subscribe(onEvent: (e: { type: 'ready' | 'ink' | 'resync'; month?: string }) => void, onClose: () => void) {
      // fetch, not EventSource: the passcode has to travel in a header
      const ctl = new AbortController()
      // the server sends a keep-alive comment every 20 s; a proxy that dies without closing the connection leaves us listening to nothing
      let last = Date.now()
      const watchdog = window.setInterval(() => {
        if (Date.now() - last > 45_000 && !ctl.signal.aborted) {
          ctl.abort()
          window.clearInterval(watchdog)
          onClose()
        }
      }, 10_000)
      void (async () => {
        try {
          const resp = await request('/api/ink/stream', { signal: ctl.signal, headers: { Accept: 'text/event-stream' } })
          const reader = resp.body!.getReader()
          const dec = new TextDecoder()
          let buf = ''
          let type = ''
          let data = ''
          for (;;) {
            const { value, done } = await reader.read()
            if (done) break
            last = Date.now()
            buf += dec.decode(value, { stream: true })
            let nl: number
            while ((nl = buf.indexOf('\n')) >= 0) {
              const line = buf.slice(0, nl).replace(/\r$/, '')
              buf = buf.slice(nl + 1)
              if (line === '') {
                if (type === 'ready' || type === 'resync') onEvent({ type })
                else if (type === 'ink') {
                  try {
                    onEvent({ type: 'ink', month: (JSON.parse(data) as { month: string }).month })
                  } catch {
                    /* a malformed frame: ignore it */
                  }
                }
                type = data = ''
              } else if (line.startsWith('event:')) type = line.slice(6).trim()
              else if (line.startsWith('data:')) data += line.slice(5).trim()
            }
          }
        } catch {
          /* closed or failed: the caller falls back to polling and reconnects */
        }
        window.clearInterval(watchdog)
        if (!ctl.signal.aborted) onClose()
      })()
      return () => {
        window.clearInterval(watchdog)
        ctl.abort()
      }
    },
    async append(month: string, ops: Record<string, unknown>[]) {
      const r = await json<{ events: { event_id: string; recorded_at: number }[] }>(`/api/ink/${month}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ ops }),
        keepalive: JSON.stringify(ops).length < 60_000, // a flush on page hide must survive the page
      })
      return r.events
    },
  },

  agents: {
    list: () => json<AgentKey[]>('/api/agents'),
    async mint(name: string) {
      const r = await request('/api/agents', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name }),
      })
      return ((await r.json()) as { ticket: string }).ticket
    },
    async revoke(id: string) {
      await request(`/api/agents/${encodeURIComponent(id)}`, { method: 'DELETE' })
    },
  },

  async setNetConfig(net: NetConfig) {
    await request('/api/net-config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(net),
    })
  },

  async changeMasterPassword(newPassword: string) {
    await request('/api/master-password/rotate', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password: newPassword }),
    })
  },

  async adoptMasterPassword(newPassword: string) {
    await request('/api/master-password/adopt', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password: newPassword }),
    })
  },

  async resetDevice() {
    // The server replies, then exits a moment later — a network error here
    // (the connection dropping mid-response) doesn't mean the reset didn't
    // happen, so it's not worth distinguishing from a clean 200.
    try {
      await request('/api/reset', { method: 'POST' })
    } catch {
      // deliberately swallowed — see above
    }
    setToken(null) // the old passcode means nothing to the blank journal that replaced it
  },

  logout() {
    setToken(null)
    window.dispatchEvent(new Event('journal:unauthorized'))
  },

  // First run on a server peer: apps/server/src/lib.rs's /api/setup/* only
  // ever answers 'ready' or 'empty' (never 'locked' — the master password
  // lives in this box's MEMORIOUS_PASSWORD env var, not typed into a
  // browser), and the passcode chosen here doubles as the bearer token, so
  // both actions log the browser straight in on success.
  setup: {
    async state() {
      return (await json<{ state: 'ready' | 'empty' }>('/api/setup/state')).state
    },
    async initFresh(passcode: string) {
      await json('/api/setup/passcode', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ passcode }),
      })
      setToken(passcode)
    },
    /** Starts the join on the server (it keeps asking the other peer for up to 15 minutes, so this is not one long request) and
     *  polls until it is done; a failure throws with the server's reason and leaves a blank client to try again. */
    async joinTicket(ticket: string, passcode: string, keep?: 'everything' | 'visible'): Promise<SyncReport> {
      await json('/api/setup/join', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ ticket, passcode, keep }),
      })
      await waitForJoin()
      setToken(passcode)
      return { received: 0, blobs: 0 }
    },
    /** A join that is already running on the server (the page was reloaded): wait for it. Returns whether one was running. */
    async joinResume(): Promise<boolean> {
      const p = await json<{ job: { state: string } }>('/api/setup/join-progress')
      if (p.job?.state !== 'running') return false
      await waitForJoin()
      return true
    },
    async joinProgress() {
      return (await json<{ progress: import('./types').JoinAsk | null }>('/api/setup/join-progress')).progress
    },
    async joinPasscode(passcode: string) {
      await json('/api/setup/join-passcode', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ passcode }),
      })
    },
    async joinCancel() {
      await json('/api/setup/join-cancel', { method: 'POST' })
    },
    async unlock() {
      throw new Error('this server has no locked state to unlock')
    },
  },

  async downloads() {
    return (await json<{ files: import('./types').DownloadFile[] }>('/api/downloads')).files
  },
}

