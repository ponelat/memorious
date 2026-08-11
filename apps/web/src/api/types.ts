export type EntryKind = 'text' | 'photo' | 'audio' | 'other'

export interface MediaRef {
  hash: string
  size: number
  url: string
}

export interface Entry {
  event_id: string
  device_id: string
  recorded_at: number
  kind: EntryKind
  text?: string
  media?: MediaRef
  /** Enrichment text (transcription/OCR), attached by the adapter when present. */
  annotation?: string
}

export interface FeedPage {
  entries: Entry[]
  next_before: number | null
}

export interface Status {
  device_id: string
  entries: number
  trash: number
  heads: Record<string, number>
  ticket?: string
}

export interface SyncReport {
  sent?: number
  received: number
  blobs: number
}

/** First-run choices on hosts that own their journal (desktop, iOS). */
export interface SetupApi {
  state(): Promise<'ready' | 'empty'>
  initFresh(): Promise<void>
  joinTicket(ticket: string): Promise<SyncReport>
}

/**
 * The one seam between the shared UI and its host. The browser build talks HTTP
 * to the server peer; the Tauri build implements the same interface with
 * commands against the embedded core.
 */
export interface JournalApi {
  /** Browser needs the passcode; a host with its own core is already trusted. */
  needsAuth: boolean
  checkPasscode(passcode: string): Promise<boolean>
  captureText(text: string): Promise<Entry>
  capturePhoto(file: Blob): Promise<Entry>
  captureAudio(file: Blob): Promise<Entry>
  feed(before?: number): Promise<FeedPage>
  mediaBlob(media: MediaRef): Promise<Blob>
  redact(eventId: string): Promise<void>
  trash(): Promise<Entry[]>
  search(q: string): Promise<Entry[]>
  status(): Promise<Status>
  /** Present only on hosts that dial peers themselves (desktop). */
  setup?: SetupApi
  syncNow?(ticket?: string): Promise<SyncReport>
  /** App builds hosted by the server peer (browser only). */
  downloads?(): Promise<DownloadFile[]>
}

export interface DownloadFile {
  name: string
  size: number
  url: string
}
