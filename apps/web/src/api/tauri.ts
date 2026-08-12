import { invoke } from '@tauri-apps/api/core'
import type { Entry, FeedPage, JournalApi, MediaRef, Status, SyncReport } from './types'

async function blobToBytes(blob: Blob): Promise<number[]> {
  return Array.from(new Uint8Array(await blob.arrayBuffer()))
}

export const tauriApi: JournalApi = {
  needsAuth: false,

  async checkPasscode() {
    return true // the desktop owns its core; no browser passcode
  },

  captureText(text: string) {
    return invoke<Entry>('capture_text', { text })
  },

  async capturePhoto(file: Blob) {
    return invoke<Entry>('capture_media', { kind: 'photo', bytes: await blobToBytes(file) })
  },

  async captureAudio(file: Blob) {
    return invoke<Entry>('capture_media', { kind: 'audio', bytes: await blobToBytes(file) })
  },

  async captureVideo(file: Blob) {
    return invoke<Entry>('capture_media', { kind: 'video', bytes: await blobToBytes(file) })
  },

  feed(before?: number) {
    return invoke<FeedPage>('feed', { before })
  },

  async mediaBlob(media: MediaRef) {
    const buf = await invoke<ArrayBuffer>('media_bytes', { hash: media.hash })
    return new Blob([buf])
  },

  async redact(eventId: string) {
    await invoke('redact', { eventId })
  },

  async trash() {
    return (await invoke<{ entries: Entry[] }>('trash_list')).entries
  },

  async search(q: string) {
    return (await invoke<{ entries: Entry[] }>('search', { q })).entries
  },

  status() {
    return invoke<Status>('status')
  },

  setup: {
    state: () => invoke<'ready' | 'empty' | 'locked'>('setup_state'),
    initFresh: (password: string) => invoke('setup_init', { password }),
    joinTicket: (ticket: string, password: string) =>
      invoke<SyncReport>('setup_join', { ticket, password }),
    unlock: (password: string) => invoke('unlock', { password }),
  },

  syncNow(ticket?: string) {
    return invoke<SyncReport>('sync_now', { ticket })
  },
}
