export type EntryKind = 'text' | 'photo' | 'audio' | 'video' | 'other'

/**
 * What an audio capture is, decided at record time (crates/core/src/retention.rs):
 * 'voice' — a dictated note; the transcript is the record, the audio may be pruned.
 * 'music' — a recording; captured clean, never transcribed, never pruned.
 */
export type AudioKind = 'voice' | 'music'

export interface MediaRef {
  hash: string
  size: number
  url: string
  /** This device no longer holds the bytes (retention); the entry still stands. */
  evicted?: boolean
}

export interface Entry {
  event_id: string
  device_id: string
  recorded_at: number
  kind: EntryKind
  /** Present on audio entries. */
  audio_kind?: AudioKind
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

/** A device's media tally: blobs its log references vs blobs it holds;
 * `evictable` is what its retention policy currently lets go, so
 * held + evictable >= referenced means complete by its own rules. */
export interface MediaHeld {
  referenced: number
  held: number
  evictable: number
  bytes: number
  policy?: unknown
}

/** A known sync peer, as fresh as our last contact with it. */
export interface PeerInfo {
  endpoint_id: string
  device_id?: string | null
  last_ok_ms: number
  /** How it was discovered: "ticket" (pairing ticket) or "inbound" (it found us). */
  discovery?: string | null
  conn?: PeerConn | null
  /** Per-device heads it held at our last handshake (the version-vector ack). */
  heads?: Record<string, number> | null
  /** Sum of `heads`; pair with Status.events_total. Null = synced before this build. */
  events_held?: number | null
  /** Events we hold that it lacked, judged live against our heads. */
  events_missing?: number | null
  /** Its own media tally at last contact. */
  media?: MediaHeld | null
  /** Referenced media it lacked beyond its retention policy. */
  media_missing?: number | null
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
  /** Peers last seen holding less than we do (events, or unexcused media). */
  peers_behind?: number
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
  /** Sum of heads — the "y" in a peer's "x of y events". */
  events_total?: number
  /** This device's own media tally. */
  media?: MediaHeld
  ticket?: string
  timeline?: TimelineStats
  storage?: StorageUsage
  health?: SyncHealth
  /** Friendly name per device id (editable, latest wins). */
  names?: Record<string, string>
  peers?: PeerInfo[]
  net?: NetConfig
}

/** One peer's answer to "can I reach you right now?" — the ordinary sync
 * handshake with a stopwatch (a behind peer is healed by the probe). */
export interface PeerPing {
  endpoint_id: string
  device_id?: string | null
  ok: boolean
  rtt_ms?: number | null
  error?: string | null
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
  captureAudio(file: Blob, kind?: AudioKind): Promise<Entry>
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
  /** Probe every known peer; resolves when all answered or timed out (~4s). */
  pingPeers(): Promise<PeerPing[]>
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
