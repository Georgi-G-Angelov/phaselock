// ── PhaseLock TypeScript types ───────────────────────────────────────────────
// These mirror the Rust event payloads sent via Tauri IPC.

export interface PeerInfo {
  peer_id: number;
  display_name: string;
}

export interface QueueItem {
  id: string;
  file_name: string;
  title: string;
  artist: string;
  duration_secs: number;
  added_by: string;
  status: 'Transferring' | 'Ready' | 'Playing' | 'Played';
}

export interface DiscoveredSession {
  session_name: string;
  host_name: string;
  address: string;
  peer_count: number;
  max_peers: number;
}

export interface SessionInfo {
  session_name: string;
  host_name: string;
  is_host: boolean;
  peer_id: number | null;
  initial_queue: QueueItem[];
}

export interface PlaybackState {
  state: 'playing' | 'paused' | 'stopped';
  file_name: string;
  position_ms: number;
  duration_ms: number;
}

export interface PlaybackPosition {
  position_ms: number;
  duration_ms: number;
}

export interface SongRequest {
  request_id: string;
  peer_name: string;
  file_name: string;
  file_size: number;
}

export interface TransferProgress {
  file_id: string;
  file_name: string;
  progress: number;
}

export interface TransferComplete {
  file_id: string;
  file_name: string;
}

export interface TransferFailed {
  file_id: string;
  file_name: string;
  error: string;
}

export interface SyncLatency {
  peer_id: number;
  latency_ms: number;
}

export interface ListenersUpdated {
  host_name: string;
  listeners: PeerInfo[];
}

export interface ErrorEvent {
  message: string;
}

export interface SyncState {
  syncing: boolean;
  message: string;
}

export interface DownloadTask {
  id: number;
  url: string;
  title: string;
  artist: string;
  status: 'searching' | 'pending' | 'fetchingMeta' | 'queued' | 'downloading' | 'failed';
  error: string | null;
}

export interface DownloadQueuePayload {
  tasks: DownloadTask[];
}

// ── Tauri event name constants ──────────────────────────────────────────────

export const EVENTS = {
  // Session
  PEER_JOINED: 'session:peer-joined',
  PEER_LEFT: 'session:peer-left',
  SESSION_ENDED: 'session:ended',
  HOST_DISCONNECTED: 'session:host-disconnected',

  // Queue
  QUEUE_UPDATED: 'queue:updated',

  // Playback
  PLAYBACK_STATE_CHANGED: 'playback:state-changed',
  PLAYBACK_POSITION: 'playback:position',
  TRACK_FINISHED: 'playback:track-finished',

  // Transfer
  TRANSFER_PROGRESS: 'transfer:progress',
  TRANSFER_COMPLETE: 'transfer:complete',
  TRANSFER_FAILED: 'transfer:failed',

  // Song requests
  REQUEST_INCOMING: 'request:incoming',
  REQUEST_UPLOAD_PROGRESS: 'request:upload-progress',

  // Discovery
  DISCOVERY_SESSIONS_UPDATED: 'discovery:sessions-updated',

  // Sync
  SYNC_LATENCY_UPDATED: 'sync:latency-updated',
  SYNC_STATE: 'sync:state',
  LISTENERS_UPDATED: 'session:listeners-updated',

  // Error
  ERROR_GENERAL: 'error:general',

  // YouTube download queue
  YOUTUBE_DOWNLOAD_QUEUE: 'youtube:download-queue',
} as const;
