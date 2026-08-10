import type { JournalApi, MediaRef } from './types'
import { httpApi } from './http'
import { tauriApi } from './tauri'

const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

export const api: JournalApi = inTauri ? tauriApi : httpApi

/**
 * <img src> / <audio src> can't send auth headers or reach Tauri commands, so
 * media is fetched through the adapter and handed out as object URLs, cached
 * by hash.
 */
const objectUrls = new Map<string, Promise<string>>()

export function mediaObjectUrl(media: MediaRef): Promise<string> {
  let p = objectUrls.get(media.hash)
  if (!p) {
    p = api.mediaBlob(media).then((b) => URL.createObjectURL(b))
    objectUrls.set(media.hash, p)
  }
  return p
}

export { getToken, setToken } from './http'
export * from './types'
