export type EntryKind = 'text' | 'photo' | 'audio' | 'video' | 'other'

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

/** The data transport a peer is reached over right now (absent between contacts). */
export interface PeerConn {
  transport: 'relay' | 'direct' | string
  /** Relay url or remote socket address. */
  detail: string
  /** Direct over a private/link-local address — same LAN. */
  lan: boolean
  /** Data flows through a middleman (relay). False = genuine p2p. */
  proxied: boolean
}

/** A known sync peer, as fresh as our last contact with it. */
export interface PeerInfo {
  endpoint_id: string
  device_id?: string | null
  last_ok_ms: number
  /** How it was discovered: "ticket" (pairing ticket) or "inbound" (it found us). */
  discovery?: string | null
  conn?: PeerConn | null
}

export interface TimelineStats {
  entries: number
  first_recorded_at: number | null
  last_recorded_at: number | null
}

export interface StorageUsage {
  db_bytes: number
  blobs_bytes: number
}

export interface SyncHealth {
  color: 'green' | 'yellow' | 'red' | string
  pending: boolean
  stalest_ms: number | null
  peers: number
}

export interface NetConfig {
  relay_mode: 'default' | 'custom' | 'disabled' | string
  relay_urls: string[]
  public_lookup: boolean
}

export interface Status {
  device_id: string
  entries: number
  trash: number
  heads: Record<string, number>
  ticket?: string
  timeline?: TimelineStats
  storage?: StorageUsage
  health?: SyncHealth
  /** Friendly name per device id (editable, latest wins). */
  names?: Record<string, string>
  peers?: PeerInfo[]
  net?: NetConfig
}

export interface SyncReport {
  sent?: number
  received: number
  blobs: number
}

/** First-run choices on hosts that own their journal (desktop, iOS). */
export interface SetupApi {
  /** 'locked': a journal exists but needs the master password this launch. */
  state(): Promise<'ready' | 'empty' | 'locked'>
  initFresh(password: string): Promise<void>
  joinTicket(ticket: string, password: string): Promise<SyncReport>
  unlock(password: string): Promise<void>
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
  captureVideo(file: Blob): Promise<Entry>
  feed(before?: number): Promise<FeedPage>
  mediaBlob(media: MediaRef): Promise<Blob>
  redact(eventId: string): Promise<void>
  trash(): Promise<Entry[]>
  search(q: string): Promise<Entry[]>
  status(): Promise<Status>
  /** Rename a device (any device — names sync). */
  setDeviceName(deviceId: string, name: string): Promise<void>
  /** Store relay/lookup config; the node applies it on next launch. */
  setNetConfig(net: NetConfig): Promise<void>
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
