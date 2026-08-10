import type { JournalApi } from './types'
import { httpApi } from './http'

// The Tauri build swaps this for a command-backed adapter (M3); the seam is
// just this constant.
export const api: JournalApi = httpApi

export { getToken, setToken, mediaObjectUrl } from './http'
export * from './types'
