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
  /** Search results only: why this entry matched. */
  match?: { keyword: boolean; similarity: number | null }
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

/** One device's self-announced build (version, git commit, compile date),
 * from Status.builds — keyed by device_id, includes this device's own
 * entry too. */
export interface PeerBuild {
  version: string
  /** `null` for an announcement from before this engine round
   * (2026-09-21) added git_sha/build_date to version_seen's payload — an
   * older peer, not a broken one. */
  git_sha: string | null
  build_date: string | null
  /** True when this device's own version is strictly newer than this
   * peer's — same comparison as the journal-wide "update available"
   * banner, applied per row. */
  behind: boolean
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

/** An MCP agent key (never the secret itself — only shown once, inside the ticket). */
export interface AgentKey {
  id: string
  name: string
  created_at: number
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
  /** device_id → when it first joined (unix ms), from the peer_join event log. */
  joins?: Record<string, number>
  peers?: PeerInfo[]
  net?: NetConfig
  /** This device's own running version (Cargo package version). */
  version?: string
  /** The git commit this build was compiled from (short SHA, `-dirty` for
   * an uncommitted worktree, "unknown" for a `.git`-less package build). */
  git_sha?: string
  /** UTC date this build was compiled (`YYYY-MM-DD`), or "unknown". */
  build_date?: string
  /** device_id → that device's own latest announced build (incl. this
   * device) — the Peers page's per-row version/sha/date and "who's out of
   * date", from the version_seen event log. */
  builds?: Record<string, PeerBuild>
  /** Set when some known peer announced a version newer than this
   * device's own — the Peers page's "update available" banner. */
  newer_version?: string | null
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

/** A markdown mirror written by a desktop export: where, and how much. */
export interface ExportReport {
  path: string
  days: number
  media: number
}

export interface BackgroundStatus {
  mode: 'direct' | 'background' | 'stopped'
  error?: string
}

/** Optional desktop PTT is deliberately daemon-only; secrets never come back
 * to the webview. The worker field is its local, non-journal health report. */
export interface PttStatus {
  available: boolean
  supported: boolean
  enabled: boolean
  key: string
  stt_model: string
  cleanup_model: string
  api_key_configured: boolean
  worker?: { state?: string; queued?: number; error?: string } | null
  restart_required: boolean
}

export interface PttConfig {
  enabled: boolean
  key: string
  /** Omit to preserve the existing key; empty clears it. */
  api_key?: string
  stt_model: string
  cleanup_model: string
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
  /** Words from the journal near the query in meaning (peers with embeddings only). */
  related?(q: string): Promise<string[]>
  status(): Promise<Status>
  /** Rename a device (any device — names sync). */
  setDeviceName(deviceId: string, name: string): Promise<void>
  /** Store relay/lookup config; the node applies it on next launch. */
  setNetConfig(net: NetConfig): Promise<void>
  /** Probe every known peer; resolves when all answered or timed out (~4s). */
  pingPeers(): Promise<PeerPing[]>
  /** Originate a master-password change on this device: re-wraps existing
   * media, re-keys the local database, and starts using it immediately.
   * Every other device must separately call `adoptMasterPassword` once
   * told the new password (out-of-band — it never travels in a ticket). */
  changeMasterPassword(newPassword: string): Promise<void>
  /** Catch up to a rotation another device already published. Rejects a
   * wrong guess without changing anything. */
  adoptMasterPassword(newPassword: string): Promise<void>
  /** MCP over QUIC: let a chat client reach this peer through `memorious mcp-bridge`.
   * `mint` returns the agent ticket — the only time its key is visible. */
  agents?: {
    list(): Promise<AgentKey[]>
    mint(name: string): Promise<string>
    revoke(id: string): Promise<void>
  }
  /** Present only on hosts that dial peers themselves (desktop). */
  setup?: SetupApi
  syncNow?(ticket?: string): Promise<SyncReport>
  /** Mirror the journal as markdown by day plus media into a folder (desktop only). */
  exportJournal?(): Promise<ExportReport>
  /** Delete this device's (or, over the browser, this server's) copy of the
   * journal and its identity; other devices keep theirs. On the browser
   * this also ends the server process — it takes longer to come back than
   * on desktop, where the app just returns to first-run setup in place. */
  resetDevice?(): Promise<void>
  /** Desktop-only process handoff; browsers never own a local journal. */
  background?: {
    status(): Promise<BackgroundStatus>
    enable(): Promise<void>
    disable(): Promise<void>
    reclaim(): Promise<void>
  }
  ptt?: {
    status(): Promise<PttStatus>
    configure(config: PttConfig): Promise<void>
  }
  /** Forget the cached passcode and return to the passcode screen — doesn't
   * touch the server or the journal at all (browser only; a host with its
   * own core has no passcode to forget). */
  logout?(): void
  /** App builds hosted by the server peer (browser only). */
  downloads?(): Promise<DownloadFile[]>
}

export interface DownloadFile {
  name: string
  size: number
  url: string
}
