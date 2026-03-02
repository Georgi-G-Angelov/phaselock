pub mod helpers;
pub mod playback;
pub mod queue;
pub mod requests;
pub mod session;

// Re-export items used directly from lib.rs.
pub use self::playback::{setup_auto_advance_listener, stop_position_ticker};

use crate::audio::decoder::DecodedAudio;
use crate::audio::playback::AudioOutput;
use crate::network::mdns::MdnsBrowser;
use crate::network::messages::{CurrentTrack, QueueItem};
use crate::network::tcp::TcpHost;
use crate::queue::manager::QueueManager;
use crate::session::Session;
use crate::transfer::file_cache::FileCache;
use crate::transfer::file_transfer::{FileReceiver, FileTransferManager};
use crate::transfer::song_request::SongRequestManager;
use parking_lot::Mutex as ParkingMutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

// ── Event payload types ─────────────────────────────────────────────────────

#[derive(Clone, Serialize)]
pub(super) struct PeerJoinedPayload {
    pub peer_id: u32,
    pub display_name: String,
}

#[derive(Clone, Serialize)]
pub(super) struct PeerLeftPayload {
    pub peer_id: u32,
}

#[derive(Clone, Serialize)]
pub(super) struct QueueUpdatedPayload {
    pub queue: Vec<QueueItem>,
}

#[derive(Clone, Serialize)]
pub(super) struct PlaybackStatePayload {
    pub state: String,
    pub file_name: String,
    pub position_ms: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Serialize)]
pub(super) struct PlaybackPositionPayload {
    pub position_ms: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Serialize)]
pub(super) struct TransferProgressPayload {
    pub file_id: String,
    pub file_name: String,
    pub progress: f64,
}

#[derive(Clone, Serialize)]
pub(super) struct TransferCompletePayload {
    pub file_id: String,
    pub file_name: String,
}

#[derive(Clone, Serialize)]
pub(super) struct TransferFailedPayload {
    pub file_id: String,
    pub file_name: String,
    pub error: String,
}

#[derive(Clone, Serialize)]
pub(super) struct RequestIncomingPayload {
    pub request_id: String,
    pub peer_name: String,
    pub file_name: String,
    pub file_size: u64,
}

#[derive(Clone, Serialize)]
pub(super) struct RequestUploadProgressPayload {
    pub request_id: String,
    pub progress: f64,
}

#[derive(Clone, Serialize)]
pub struct DiscoveredSessionPayload {
    pub session_name: String,
    pub host_name: String,
    pub address: String,
    pub peer_count: u8,
    pub max_peers: u8,
}

#[derive(Clone, Serialize)]
pub(super) struct DiscoveryPayload {
    pub sessions: Vec<DiscoveredSessionPayload>,
}

#[derive(Clone, Serialize)]
pub(super) struct SyncLatencyPayload {
    pub peer_id: u32,
    pub latency_ms: f64,
}

#[derive(Clone, Serialize)]
pub(super) struct ErrorPayload {
    pub message: String,
}

#[derive(Clone, Serialize)]
pub(super) struct SyncStatePayload {
    pub syncing: bool,
    pub message: String,
}

#[derive(Clone, Serialize)]
pub(super) struct ListenerInfo {
    pub peer_id: u32,
    pub display_name: String,
}

#[derive(Clone, Serialize)]
pub(super) struct ListenersUpdatedPayload {
    pub host_name: String,
    pub listeners: Vec<ListenerInfo>,
}

#[derive(Clone, Serialize)]
pub struct SessionInfoPayload {
    pub session_name: String,
    pub host_name: String,
    pub is_host: bool,
    pub peer_id: Option<u32>,
    pub initial_queue: Vec<QueueItem>,
}

// ── App State ───────────────────────────────────────────────────────────────

/// Shared application state, accessible from all Tauri commands.
pub struct AppState {
    pub session: Mutex<Session>,
    pub queue: Mutex<QueueManager>,
    pub song_requests: Mutex<SongRequestManager>,
    pub volume: ParkingMutex<f32>,
    pub mdns_browser: Mutex<Option<MdnsBrowser>>,
    /// Handle to the position ticker task (so we can abort it).
    pub position_ticker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Peer-side file cache — persists across reconnects within the same app session.
    pub file_cache: Mutex<FileCache>,
    /// Host-side shared queue state (kept in sync with QueueManager for session broadcasts).
    pub host_queue_state: std::sync::Arc<ParkingMutex<Vec<QueueItem>>>,
    /// Host-side shared current track state (for session broadcasts).
    pub host_current_track: std::sync::Arc<ParkingMutex<Option<CurrentTrack>>>,
    /// Audio output engine (created lazily on first play).
    pub audio_output: Mutex<Option<AudioOutput>>,
    /// Raw file bytes for each queued track (keyed by track UUID).
    pub track_data: Mutex<HashMap<Uuid, Vec<u8>>>,
    /// Host's TCP handle — used to broadcast queue/playback updates to peers.
    pub host_tcp: ParkingMutex<Option<Arc<TcpHost>>>,
    /// Host-side file transfer manager — sends files to peers.
    pub file_transfer_mgr: Mutex<FileTransferManager>,
    /// Peer-side file receiver — reassembles incoming file chunks.
    pub file_receiver: Mutex<FileReceiver>,
    /// Peer-side decoded audio cache — pre-decoded tracks ready for instant playback.
    pub decoded_cache: Mutex<HashMap<Uuid, DecodedAudio>>,
    /// Cached device sample rate — set when AudioOutput is first created, used for pre-resampling.
    pub device_sample_rate: ParkingMutex<Option<u32>>,
    /// Handle to the latency-logging ticker task (aborted on session leave).
    pub latency_ticker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Peer-side: file_id of the track currently loaded/playing in AudioOutput.
    pub peer_current_file_id: ParkingMutex<Option<Uuid>>,
    /// Peer-side: maps file_name → file_path for pending song requests awaiting host accept.
    pub pending_request_paths: ParkingMutex<HashMap<String, String>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(Session::None),
            queue: Mutex::new(QueueManager::new()),
            song_requests: Mutex::new(SongRequestManager::new()),
            volume: ParkingMutex::new(1.0),
            mdns_browser: Mutex::new(None),
            position_ticker: Mutex::new(None),
            file_cache: Mutex::new(FileCache::new()),
            host_queue_state: std::sync::Arc::new(ParkingMutex::new(Vec::new())),
            host_current_track: std::sync::Arc::new(ParkingMutex::new(None)),
            audio_output: Mutex::new(None),
            track_data: Mutex::new(HashMap::new()),
            host_tcp: ParkingMutex::new(None),
            file_transfer_mgr: Mutex::new(FileTransferManager::new()),
            file_receiver: Mutex::new(FileReceiver::new()),
            decoded_cache: Mutex::new(HashMap::new()),
            device_sample_rate: ParkingMutex::new(None),
            latency_ticker: Mutex::new(None),
            peer_current_file_id: ParkingMutex::new(None),
            pending_request_paths: ParkingMutex::new(HashMap::new()),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
