import { invoke } from '@tauri-apps/api/core'
import type { Entry, FeedPage, JournalApi, MediaRef, NetConfig, Status, SyncReport } from './types'

/**
 * Media goes over as a raw request body, never as a JSON array of numbers: a
 * pasted screenshot is megabytes, and the number-array shape costs ~30x that in
 * transient allocation — enough to take the Linux webview's web process down.
 * The kind rides along in a header (see `capture_media` in the Tauri shell).
 */
async function captureMedia(kind: 'photo' | 'audio' | 'video', blob: Blob): Promise<Entry> {
  return invoke<Entry>('capture_media', await blob.arrayBuffer(), {
    headers: { 'media-kind': kind },
  })
}

export const tauriApi: JournalApi = {
  needsAuth: false,

  async checkPasscode() {
    return true // the desktop owns its core; no browser passcode
  },

  captureText(text: string) {
    return invoke<Entry>('capture_text', { text })
  },

  capturePhoto(file: Blob) {
    return captureMedia('photo', file)
  },

  captureAudio(file: Blob) {
    return captureMedia('audio', file)
  },

  captureVideo(file: Blob) {
    return captureMedia('video', file)
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

  async setDeviceName(deviceId: string, name: string) {
    await invoke('set_device_name', { deviceId, name })
  },

  async setNetConfig(net: NetConfig) {
    await invoke('set_net_config', { net })
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
