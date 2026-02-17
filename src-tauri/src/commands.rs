use crate::network::mdns::{DiscoveredSession, MdnsBrowser};
use crate::network::messages::{CurrentTrack, QueueItem};
use crate::queue::manager::QueueManager;
use crate::session::Session;
use crate::transfer::file_cache::FileCache;
use crate::transfer::song_request::SongRequestManager;
use parking_lot::Mutex as ParkingMutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;
use uuid::Uuid;

// ── Event payload types ─────────────────────────────────────────────────────

#[derive(Clone, Serialize)]
struct PeerJoinedPayload {
    peer_id: u32,
    display_name: String,
}

#[derive(Clone, Serialize)]
struct PeerLeftPayload {
    peer_id: u32,
}

#[derive(Clone, Serialize)]
struct QueueUpdatedPayload {
    queue: Vec<QueueItem>,
}

#[derive(Clone, Serialize)]
struct PlaybackStatePayload {
    state: String,
    file_name: String,
    position_ms: u64,
    duration_ms: u64,
}

#[derive(Clone, Serialize)]
struct PlaybackPositionPayload {
    position_ms: u64,
    duration_ms: u64,
}

#[derive(Clone, Serialize)]
struct TransferProgressPayload {
    file_id: String,
    file_name: String,
    progress: f64,
}

#[derive(Clone, Serialize)]
struct TransferCompletePayload {
    file_id: String,
    file_name: String,
}

#[derive(Clone, Serialize)]
struct TransferFailedPayload {
    file_id: String,
    file_name: String,
    error: String,
}

#[derive(Clone, Serialize)]
struct RequestIncomingPayload {
    request_id: String,
    peer_name: String,
    file_name: String,
    file_size: u64,
}

#[derive(Clone, Serialize)]
struct RequestUploadProgressPayload {
    request_id: String,
    progress: f64,
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
struct DiscoveryPayload {
    sessions: Vec<DiscoveredSessionPayload>,
}

#[derive(Clone, Serialize)]
struct SyncLatencyPayload {
    peer_id: u32,
    latency_ms: f64,
}

#[derive(Clone, Serialize)]
struct ErrorPayload {
    message: String,
}

#[derive(Clone, Serialize)]
struct SyncStatePayload {
    syncing: bool,
    message: String,
}

#[derive(Clone, Serialize)]
pub struct SessionInfoPayload {
    pub session_name: String,
    pub host_name: String,
    pub is_host: bool,
    pub peer_id: Option<u32>,
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
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helper ──────────────────────────────────────────────────────────────────

fn emit_error(app: &AppHandle, msg: &str) {
    let _ = app.emit("error:general", ErrorPayload { message: msg.to_string() });
}

fn emit_queue_update(app: &AppHandle, queue: &[QueueItem]) {
    // Also update the shared host queue state so HostSession broadcasts it.
    let state = app.state::<AppState>();
    *state.host_queue_state.lock() = queue.to_vec();

    let _ = app.emit(
        "queue:updated",
        QueueUpdatedPayload {
            queue: queue.to_vec(),
        },
    );
}

fn discovered_to_payload(d: &DiscoveredSession) -> DiscoveredSessionPayload {
    DiscoveredSessionPayload {
        session_name: d.session_name.clone(),
        host_name: d.host_name.clone(),
        address: d.address.to_string(),
        peer_count: d.peer_count,
        max_peers: d.max_peers,
    }
}

// ── Session Commands ────────────────────────────────────────────────────────

#[tauri::command]
pub async fn create_session(
    app: AppHandle,
    session_name: String,
    display_name: String,
) -> Result<SessionInfoPayload, String> {
    // Input validation
    let session_name = session_name.trim().to_string();
    let display_name = display_name.trim().to_string();
    if session_name.is_empty() {
        return Err("Session name cannot be empty.".into());
    }
    if session_name.len() > 50 {
        return Err("Session name must be 50 characters or fewer.".into());
    }
    if display_name.is_empty() {
        return Err("Display name cannot be empty.".into());
    }
    if display_name.len() > 30 {
        return Err("Display name must be 30 characters or fewer.".into());
    }

    let state = app.state::<AppState>();
    let mut session = state.session.lock().await;

    if session.is_active() {
        return Err("A session is already active. Leave first.".into());
    }

    use crate::session::host::HostSession;
    let tcp_port = 17401u16;
    let udp_port = 17402u16;

    match HostSession::start(session_name.clone(), display_name.clone(), tcp_port, udp_port, false).await {
        Ok((mut host_session, mut event_rx)) => {
            // Wire the shared queue/track state into the host session.
            host_session.queue_state = state.host_queue_state.clone();
            host_session.current_track_state = state.host_current_track.clone();

            let info = SessionInfoPayload {
                session_name: host_session.session_name.clone(),
                host_name: host_session.host_display_name.clone(),
                is_host: true,
                peer_id: None,
            };

            *session = Session::Host(host_session);
            drop(session);

            // Spawn a task to forward SessionEvents → Tauri events.
            let app_clone = app.clone();
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    match event {
                        crate::session::SessionEvent::PeerJoined { peer_id, display_name } => {
                            let _ = app_clone.emit("session:peer-joined", PeerJoinedPayload { peer_id, display_name });
                        }
                        crate::session::SessionEvent::PeerLeft { peer_id } => {
                            let _ = app_clone.emit("session:peer-left", PeerLeftPayload { peer_id });
                        }
                        crate::session::SessionEvent::HostDisconnected => {
                            let _ = app_clone.emit("session:host-disconnected", ());
                        }
                        crate::session::SessionEvent::JoinRejected { reason } => {
                            emit_error(&app_clone, &format!("Join rejected: {reason}"));
                        }
                        crate::session::SessionEvent::MessageReceived { .. } => {
                            // Handled by the session layer internally.
                        }
                    }
                }
            });

            log::info!("Created session \"{session_name}\"");
            Ok(info)
        }
        Err(e) => {
            let msg = e.to_string();
            log::error!("Failed to create session: {msg}");
            if msg.contains("address already in use") || msg.contains("AddrInUse") || msg.contains("Address already in use") {
                Err("Port 17401 is already in use. Please close the other application or try again.".into())
            } else {
                Err(format!("Failed to create session: {msg}"))
            }
        }
    }
}

#[tauri::command]
pub async fn join_session(
    app: AppHandle,
    address: String,
    display_name: String,
) -> Result<SessionInfoPayload, String> {
    // Input validation
    let display_name = display_name.trim().to_string();
    if display_name.is_empty() {
        return Err("Display name cannot be empty.".into());
    }
    if display_name.len() > 30 {
        return Err("Display name must be 30 characters or fewer.".into());
    }
    let address = address.trim().to_string();
    if address.is_empty() {
        return Err("Address cannot be empty.".into());
    }

    let state = app.state::<AppState>();
    let mut session = state.session.lock().await;

    if session.is_active() {
        return Err("A session is already active. Leave first.".into());
    }

    use crate::session::peer::PeerSession;
    use std::net::SocketAddr;

    let tcp_addr: SocketAddr = address.parse().map_err(|e| format!("Invalid address: {e}"))?;
    // UDP port is conventionally TCP port + 1.
    let udp_addr = SocketAddr::new(tcp_addr.ip(), tcp_addr.port() + 1);

    match PeerSession::join(tcp_addr, udp_addr, display_name.clone(), false).await {
        Ok((mut peer_session, mut event_rx)) => {
            let info = SessionInfoPayload {
                session_name: peer_session.session_name.clone(),
                host_name: peer_session.host_name.clone(),
                is_host: false,
                peer_id: Some(peer_session.peer_id),
            };

            // Emit initial queue from session state.
            let initial_queue = peer_session.get_queue();
            if !initial_queue.is_empty() {
                emit_queue_update(&app, &initial_queue);
            }

            // Emit initial current track / playback state.
            if let Some(track) = peer_session.get_current_track() {
                let position_ms = if track.sample_rate > 0 {
                    (track.position_samples as f64 / track.sample_rate as f64 * 1000.0) as u64
                } else {
                    0
                };
                let _ = app.emit(
                    "playback:state-changed",
                    PlaybackStatePayload {
                        state: if track.is_playing { "playing".into() } else { "paused".into() },
                        file_name: track.file_name.clone(),
                        position_ms,
                        duration_ms: 0,
                    },
                );
            }

            // Send FileCacheReport to host.
            {
                let cache = state.file_cache.lock().await;
                let cached_ids = cache.cached_ids();
                peer_session.send_file_cache_report(cached_ids).await;
            }

            // Emit syncing state — peer may need files.
            let needs_sync = !initial_queue.is_empty();
            if needs_sync {
                let _ = app.emit("sync:state", SyncStatePayload {
                    syncing: true,
                    message: "Syncing with session...".into(),
                });
            }

            *session = Session::Peer(peer_session);
            drop(session);

            // Forward events.
            let app_clone = app.clone();
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    match event {
                        crate::session::SessionEvent::PeerJoined { peer_id, display_name } => {
                            let _ = app_clone.emit("session:peer-joined", PeerJoinedPayload { peer_id, display_name });
                        }
                        crate::session::SessionEvent::PeerLeft { peer_id } => {
                            let _ = app_clone.emit("session:peer-left", PeerLeftPayload { peer_id });
                        }
                        crate::session::SessionEvent::HostDisconnected => {
                            let _ = app_clone.emit("session:host-disconnected", ());
                            // Auto-leave when host disconnects.
                            let s = app_clone.state::<AppState>();
                            let mut sess = s.session.lock().await;
                            *sess = Session::None;
                        }
                        crate::session::SessionEvent::JoinRejected { reason } => {
                            emit_error(&app_clone, &format!("Join rejected: {reason}"));
                        }
                        crate::session::SessionEvent::MessageReceived { .. } => {}
                    }
                }
            });

            log::info!("Joined session at {address}");
            Ok(info)
        }
        Err(e) => {
            let msg = e.to_string();
            log::error!("Failed to join session: {msg}");
            if msg.contains("timed out") || msg.contains("Timeout") {
                Err("Connection timed out. Make sure the host is running and reachable.".into())
            } else if msg.contains("Connection refused") || msg.contains("refused") {
                Err("Connection refused. Make sure the host is running on the specified address.".into())
            } else if msg.contains("full") {
                Err("Session is full. The host has reached the maximum number of peers.".into())
            } else {
                Err(format!("Failed to join session: {msg}"))
            }
        }
    }
}

#[tauri::command]
pub async fn leave_session(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut session = state.session.lock().await;

    if !session.is_active() {
        return Err("No active session to leave.".into());
    }

    session.shutdown().await;
    let _ = app.emit("session:ended", ());
    log::info!("Left session");
    Ok(())
}

#[tauri::command]
pub async fn get_discovered_sessions(
    app: AppHandle,
) -> Result<Vec<DiscoveredSessionPayload>, String> {
    let state = app.state::<AppState>();
    let mut browser_lock = state.mdns_browser.lock().await;

    // Start browsing if not already.
    if browser_lock.is_none() {
        match MdnsBrowser::start() {
            Ok(b) => *browser_lock = Some(b),
            Err(e) => return Err(format!("Failed to start mDNS browser: {e}")),
        }
    }

    let sessions = browser_lock
        .as_ref()
        .map(|b| {
            b.get_sessions()
                .iter()
                .map(discovered_to_payload)
                .collect()
        })
        .unwrap_or_default();

    Ok(sessions)
}

// ── Playback Commands ───────────────────────────────────────────────────────

#[tauri::command]
pub async fn play(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            // In a full implementation this would trigger the PlaybackScheduler.
            // For now, emit a state change event.
            let _ = app.emit(
                "playback:state-changed",
                PlaybackStatePayload {
                    state: "playing".into(),
                    file_name: String::new(),
                    position_ms: 0,
                    duration_ms: 0,
                },
            );
            log::info!("Play command received");
            Ok(())
        }
        _ => Err("Only the host can control playback.".into()),
    }
}

#[tauri::command]
pub async fn pause(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            stop_position_ticker(&state).await;
            let _ = app.emit(
                "playback:state-changed",
                PlaybackStatePayload {
                    state: "paused".into(),
                    file_name: String::new(),
                    position_ms: 0,
                    duration_ms: 0,
                },
            );
            log::info!("Pause command received");
            Ok(())
        }
        _ => Err("Only the host can control playback.".into()),
    }
}

#[tauri::command]
pub async fn stop(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            stop_position_ticker(&state).await;
            let _ = app.emit(
                "playback:state-changed",
                PlaybackStatePayload {
                    state: "stopped".into(),
                    file_name: String::new(),
                    position_ms: 0,
                    duration_ms: 0,
                },
            );
            log::info!("Stop command received");
            Ok(())
        }
        _ => Err("Only the host can control playback.".into()),
    }
}

#[tauri::command]
pub async fn seek(app: AppHandle, position_ms: u64) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            log::info!("Seek to {position_ms} ms");
            let _ = app.emit(
                "playback:position",
                PlaybackPositionPayload {
                    position_ms,
                    duration_ms: 0,
                },
            );
            Ok(())
        }
        _ => Err("Only the host can control playback.".into()),
    }
}

#[tauri::command]
pub async fn skip(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            let mut queue = state.queue.lock().await;
            let has_next = queue.skip().is_some();
            let queue_items = queue.get_queue();
            drop(queue);

            emit_queue_update(&app, &queue_items);

            if has_next {
                log::info!("Skipped to next track");
            } else {
                log::info!("Skip: no more tracks");
            }
            Ok(())
        }
        _ => Err("Only the host can control playback.".into()),
    }
}

// ── Queue Commands ──────────────────────────────────────────────────────────

#[tauri::command]
pub async fn add_song(app: AppHandle, file_path: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            use std::path::Path;
            let path = Path::new(&file_path);
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            // Validate file extension
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !ext.eq_ignore_ascii_case("mp3") {
                return Err("Only .mp3 files are supported.".into());
            }

            // Read the file and validate size
            let file_data = tokio::fs::read(&file_path)
                .await
                .map_err(|e| format!("Failed to read file: {e}"))?;

            const MAX_FILE_SIZE: usize = 50 * 1024 * 1024; // 50 MB
            if file_data.len() > MAX_FILE_SIZE {
                return Err("File is too large. Maximum size is 50 MB.".into());
            }

            // Validate MP3 by trying to decode
            let duration_secs = match crate::audio::decoder::decode_mp3(&file_data) {
                Ok(decoded) => decoded.duration_secs,
                Err(_e) => return Err("Invalid or corrupted MP3 file.".into()),
            };

            let mut queue = state.queue.lock().await;
            let track_id = queue.add(file_name.clone(), duration_secs, "host".into());
            queue.mark_ready(track_id);
            let queue_items = queue.get_queue();
            drop(queue);
            drop(session);

            emit_queue_update(&app, &queue_items);

            log::info!("Added song \"{}\" to queue ({})", file_name, track_id);
            Ok(())
        }
        _ => Err("Only the host can add songs directly.".into()),
    }
}

#[tauri::command]
pub async fn remove_from_queue(app: AppHandle, track_id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            let id: Uuid = track_id.parse().map_err(|e| format!("Invalid track ID: {e}"))?;
            let mut queue = state.queue.lock().await;
            if !queue.remove(id) {
                return Err("Track not found in queue.".into());
            }
            let queue_items = queue.get_queue();
            drop(queue);

            emit_queue_update(&app, &queue_items);

            log::info!("Removed track {id} from queue");
            Ok(())
        }
        _ => Err("Only the host can modify the queue.".into()),
    }
}

#[tauri::command]
pub async fn reorder_queue(app: AppHandle, from_index: usize, to_index: usize) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            let mut queue = state.queue.lock().await;
            if !queue.reorder(from_index, to_index) {
                return Err("Invalid indices for reorder.".into());
            }
            let queue_items = queue.get_queue();
            drop(queue);

            emit_queue_update(&app, &queue_items);

            log::info!("Reordered queue: {} → {}", from_index, to_index);
            Ok(())
        }
        _ => Err("Only the host can reorder the queue.".into()),
    }
}

// ── Song Request Commands ───────────────────────────────────────────────────

#[tauri::command]
pub async fn request_song(app: AppHandle, file_path: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut session = state.session.lock().await;

    match &mut *session {
        Session::Peer(peer) => {
            use crate::network::messages::Message;
            use std::path::Path;

            let path = Path::new(&file_path);
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            // Validate file extension
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !ext.eq_ignore_ascii_case("mp3") {
                return Err("Only .mp3 files are supported.".into());
            }

            let metadata = tokio::fs::metadata(&file_path)
                .await
                .map_err(|e| format!("Failed to read file metadata: {e}"))?;

            const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
            if metadata.len() > MAX_FILE_SIZE {
                return Err("File is too large. Maximum size is 50 MB.".into());
            }

            let msg = Message::SongRequest {
                file_name,
                file_size: metadata.len(),
            };

            peer.send(&msg)
                .await
                .map_err(|e| format!("Failed to send song request: {e}"))?;

            log::info!("Sent song request for \"{}\"", file_path);
            Ok(())
        }
        _ => Err("Only peers can request songs.".into()),
    }
}

#[tauri::command]
pub async fn accept_song_request(app: AppHandle, request_id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(host) => {
            let id: Uuid = request_id.parse().map_err(|e| format!("Invalid request ID: {e}"))?;
            let mut requests = state.song_requests.lock().await;

            let req = requests.accept(id).ok_or("Request not found.")?;

            // Send acceptance to the requesting peer.
            let msg = crate::network::messages::Message::SongRequestAccepted { request_id: id };
            host.send_to_peer(req.peer_id, &msg).await;

            log::info!("Accepted song request {} from peer {}", id, req.peer_id);
            Ok(())
        }
        _ => Err("Only the host can accept song requests.".into()),
    }
}

#[tauri::command]
pub async fn reject_song_request(app: AppHandle, request_id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(host) => {
            let id: Uuid = request_id.parse().map_err(|e| format!("Invalid request ID: {e}"))?;
            let mut requests = state.song_requests.lock().await;

            let req = requests.reject(id).ok_or("Request not found.")?;

            // Send rejection to the requesting peer.
            let msg = crate::network::messages::Message::SongRequestRejected { request_id: id };
            host.send_to_peer(req.peer_id, &msg).await;

            log::info!("Rejected song request {} from peer {}", id, req.peer_id);
            Ok(())
        }
        _ => Err("Only the host can reject song requests.".into()),
    }
}

// ── Local Controls ──────────────────────────────────────────────────────────

#[tauri::command]
pub async fn set_volume(app: AppHandle, volume: f32) -> Result<(), String> {
    let state = app.state::<AppState>();
    let clamped = volume.clamp(0.0, 1.0);
    *state.volume.lock() = clamped;
    log::info!("Volume set to {clamped:.2}");
    Ok(())
}

// ── Playback Position Ticker ────────────────────────────────────────────────

/// Start a background task that emits `playback:position` every 500 ms.
pub async fn start_position_ticker(
    app: &AppHandle,
    state: &AppState,
    duration_ms: u64,
) {
    // Stop any existing ticker first.
    stop_position_ticker(state).await;

    let app_clone = app.clone();
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
        let mut position_ms: u64 = 0;

        loop {
            interval.tick().await;
            position_ms += 500;

            if position_ms > duration_ms {
                let _ = app_clone.emit("playback:track-finished", ());
                break;
            }

            let _ = app_clone.emit(
                "playback:position",
                PlaybackPositionPayload {
                    position_ms,
                    duration_ms,
                },
            );
        }
    });

    *state.position_ticker.lock().await = Some(handle);
}

/// Stop the playback position ticker if running.
pub async fn stop_position_ticker(state: &AppState) {
    let mut ticker = state.position_ticker.lock().await;
    if let Some(handle) = ticker.take() {
        handle.abort();
    }
}
