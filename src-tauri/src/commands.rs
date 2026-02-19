use crate::audio::decoder::DecodedAudio;
use crate::audio::playback::AudioOutput;
use crate::network::mdns::{DiscoveredSession, MdnsBrowser};
use crate::network::messages::{CurrentTrack, Message, QueueItem};
use crate::network::tcp::TcpHost;
use crate::network::udp::now_ns;
use crate::queue::manager::QueueManager;
use crate::session::Session;
use crate::transfer::file_cache::FileCache;
use crate::transfer::file_transfer::{FileReceiver, FileTransferManager};
use crate::transfer::song_request::SongRequestManager;
use parking_lot::Mutex as ParkingMutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
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

    // Broadcast queue update to all connected peers over TCP.
    log::info!("[emit_queue_update] {} items, broadcasting to peers", queue.len());
    let tcp_host = state.host_tcp.lock().clone();
    if let Some(tcp_host) = tcp_host {
        let msg = Message::QueueUpdate { queue: queue.to_vec() };
        tokio::spawn(async move {
            tcp_host.broadcast(&msg).await;
        });
    } else {
        log::warn!("[emit_queue_update] host_tcp is None — queue update not sent to peers");
    }
}

fn emit_playback_state(app: &AppHandle, pstate: &str, file_name: &str, position_ms: u64, duration_ms: u64) {
    log::info!("[emit_playback_state] state={pstate} file={file_name} pos={position_ms} dur={duration_ms}");
    let _ = app.emit(
        "playback:state-changed",
        PlaybackStatePayload {
            state: pstate.to_string(),
            file_name: file_name.to_string(),
            position_ms,
            duration_ms,
        },
    );

    // Broadcast to peers over TCP.
    let state = app.state::<AppState>();
    let tcp_host = state.host_tcp.lock().clone();
    if let Some(tcp_host) = tcp_host {
        let msg = Message::PlaybackStateUpdate {
            state: pstate.to_string(),
            file_name: file_name.to_string(),
            position_ms,
            duration_ms,
        };
        tokio::spawn(async move {
            tcp_host.broadcast(&msg).await;
        });
    }
}

/// Broadcast any message to all connected peers (no-op if not host).
fn broadcast_to_peers(app: &AppHandle, msg: &Message) {
    let state = app.state::<AppState>();
    let tcp_host = state.host_tcp.lock().clone();
    if let Some(tcp_host) = tcp_host {
        log::debug!("[broadcast_to_peers] Sending {:?}", std::mem::discriminant(msg));
        let msg = msg.clone();
        tokio::spawn(async move {
            tcp_host.broadcast(&msg).await;
        });
    } else {
        log::warn!("[broadcast_to_peers] host_tcp is None — cannot broadcast");
    }
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

    match HostSession::start_with_state(
        session_name.clone(),
        display_name.clone(),
        tcp_port,
        udp_port,
        false,
        Some(state.host_queue_state.clone()),
        Some(state.host_current_track.clone()),
    ).await {
        Ok((host_session, mut event_rx)) => {

            // Store TCP handle so emit_queue_update can broadcast to peers.
            *state.host_tcp.lock() = Some(host_session.tcp_host.clone());

            let info = SessionInfoPayload {
                session_name: host_session.session_name.clone(),
                host_name: host_session.host_display_name.clone(),
                is_host: true,
                initial_queue: Vec::new(),
                peer_id: None,
            };

            *session = Session::Host(host_session);
            drop(session);

            // Eagerly initialise AudioOutput so play_current_track is instant.
            {
                let mut audio = state.audio_output.lock().await;
                if audio.is_none() {
                    match AudioOutput::new() {
                        Ok(ao) => {
                            *state.device_sample_rate.lock() = Some(ao.device_sample_rate);
                            log::info!("[create_session] AudioOutput initialised eagerly — device rate = {} Hz", ao.device_sample_rate);
                            *audio = Some(ao);
                        }
                        Err(e) => {
                            log::warn!("[create_session] Failed to eagerly init AudioOutput: {e} — will init on first play");
                        }
                    }
                }
            }

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
                        crate::session::SessionEvent::MessageReceived { peer_id, message } => {
                            match message {
                                Message::FileCacheReport { file_ids } => {
                                    // Peer told us what files it already has.
                                    // Transfer any queued tracks the peer is missing.
                                    log::info!("[host] Received FileCacheReport from peer {peer_id} with {} cached file(s)", file_ids.len());
                                    let s = app_clone.state::<AppState>();
                                    let queue = s.queue.lock().await;
                                    let all_track_ids: Vec<Uuid> = queue.get_queue().iter().map(|q| q.id).collect();
                                    log::info!("[host] Queue has {} track(s): {:?}", all_track_ids.len(), all_track_ids);
                                    drop(queue);

                                    let cached_set: std::collections::HashSet<Uuid> = file_ids.into_iter().collect();
                                    let missing: Vec<Uuid> = all_track_ids.into_iter()
                                        .filter(|id| !cached_set.contains(id))
                                        .collect();

                                    if !missing.is_empty() {
                                        log::info!("[host] Peer {peer_id} is missing {} file(s): {:?}", missing.len(), missing);
                                        let tcp_host = s.host_tcp.lock().clone();
                                        let track_store = s.track_data.lock().await;
                                        log::info!("[host] track_data has {} entries", track_store.len());
                                        if let Some(tcp_host) = tcp_host {
                                            let mut mgr = s.file_transfer_mgr.lock().await;
                                            for file_id in missing {
                                                if let Some(data) = track_store.get(&file_id) {
                                                    let queue = s.queue.lock().await;
                                                    let file_name = queue.get_queue().iter()
                                                        .find(|q| q.id == file_id)
                                                        .map(|q| q.file_name.clone())
                                                        .unwrap_or_default();
                                                    drop(queue);
                                                    if let Err(e) = mgr.start_transfer_with_id(
                                                        file_id,
                                                        file_name.clone(),
                                                        data.clone(),
                                                        &[peer_id],
                                                        &tcp_host,
                                                    ).await {
                                                        log::warn!("Failed to transfer \"{file_name}\" to peer {peer_id}: {e}");
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        log::info!("Peer {peer_id} already has all files");
                                    }
                                }
                                _ => {
                                    // Other messages handled by session layer internally.
                                }
                            }
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
            let initial_queue = peer_session.get_queue();
            log::info!("join_session: peer has {} items in initial queue", initial_queue.len());
            for item in &initial_queue {
                log::info!("  queue item: {} - {:?}", item.file_name, item.status);
            }
            let info = SessionInfoPayload {
                session_name: peer_session.session_name.clone(),
                host_name: peer_session.host_name.clone(),
                is_host: false,
                peer_id: Some(peer_session.peer_id),
                initial_queue: initial_queue.clone(),
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
                log::info!("[join_session] Sending FileCacheReport to host with {} cached file(s): {:?}", cached_ids.len(), cached_ids);
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

            // Eagerly initialise AudioOutput so we know the device sample rate
            // before any PlayCommand arrives.  This lets the pre-decode task
            // resample to the device rate in the background.
            {
                let mut audio = state.audio_output.lock().await;
                if audio.is_none() {
                    match AudioOutput::new() {
                        Ok(ao) => {
                            *state.device_sample_rate.lock() = Some(ao.device_sample_rate);
                            log::info!("[join_session] AudioOutput initialised eagerly — device rate = {} Hz", ao.device_sample_rate);
                            *audio = Some(ao);
                        }
                        Err(e) => {
                            log::warn!("[join_session] Failed to eagerly init AudioOutput: {e} — will init on first PlayCommand");
                        }
                    }
                }
            }

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
                        crate::session::SessionEvent::MessageReceived { message, .. } => {
                            match message {
                                Message::QueueUpdate { queue } => {
                                    log::info!("[peer] Received QueueUpdate: {} item(s)", queue.len());
                                    let _ = app_clone.emit(
                                        "queue:updated",
                                        QueueUpdatedPayload { queue },
                                    );
                                }
                                Message::PlaybackStateUpdate { state, file_name, position_ms, duration_ms } => {
                                    log::info!("[peer] Received PlaybackStateUpdate: state={state} file={file_name} pos={position_ms} dur={duration_ms}");
                                    let _ = app_clone.emit(
                                        "playback:state-changed",
                                        PlaybackStatePayload {
                                            state,
                                            file_name,
                                            position_ms,
                                            duration_ms,
                                        },
                                    );
                                }
                                Message::FileTransferStart { file_id, file_name, size, sha256 } => {
                                    log::info!("[peer] Received FileTransferStart: id={file_id} name={file_name} size={size}");
                                    let s = app_clone.state::<AppState>();
                                    let mut receiver = s.file_receiver.lock().await;
                                    if let Err(e) = receiver.handle_transfer_start(file_id, file_name.clone(), size, sha256) {
                                        log::warn!("Failed to start receiving file \"{file_name}\": {e}");
                                    }
                                }
                                Message::FileChunk { file_id, offset, data } => {
                                    log::debug!("[peer] Received FileChunk: id={file_id} offset={offset} chunk_len={}", data.len());
                                    let s = app_clone.state::<AppState>();
                                    let mut receiver = s.file_receiver.lock().await;
                                    if let Some((event, response)) = receiver.handle_chunk(file_id, offset, &data) {
                                        // Store completed file in the file cache.
                                        if let crate::transfer::file_transfer::TransferEvent::FileReady { file_id, file_name } = &event {
                                            if let Some(file_data) = receiver.get_file(*file_id) {
                                                let mut cache = s.file_cache.lock().await;
                                                cache.insert(*file_id, file_data.clone());
                                                log::info!("Cached received file \"{file_name}\" ({file_id})");

                                                // Pre-decode (and pre-resample) in background so PlayCommand is instant.
                                                let fid = *file_id;
                                                let fname = file_name.clone();
                                                let bytes = file_data.clone();
                                                let app_for_decode = app_clone.clone();
                                                let target_rate = *s.device_sample_rate.lock();
                                                tokio::spawn(async move {
                                                    log::info!("[peer] Pre-decoding \"{fname}\" ({fid})...");
                                                    match tokio::task::spawn_blocking(move || {
                                                        let decoded = crate::audio::decoder::decode_mp3(&bytes)?;
                                                        // If we know the device sample rate and it differs,
                                                        // resample now so load_track is instant later.
                                                        if let Some(dev_rate) = target_rate {
                                                            if decoded.sample_rate != dev_rate {
                                                                log::info!(
                                                                    "[peer] Pre-resampling \"{fname}\" from {} Hz to {} Hz",
                                                                    decoded.sample_rate, dev_rate
                                                                );
                                                                let resampled = AudioOutput::resample(
                                                                    &decoded.samples,
                                                                    decoded.channels,
                                                                    decoded.sample_rate,
                                                                    dev_rate,
                                                                );
                                                                let new_frames = resampled.len() as u64 / decoded.channels as u64;
                                                                let new_duration = new_frames as f64 / dev_rate as f64;
                                                                return Ok(crate::audio::decoder::DecodedAudio {
                                                                    samples: resampled,
                                                                    sample_rate: dev_rate,
                                                                    channels: decoded.channels,
                                                                    total_frames: new_frames,
                                                                    duration_secs: new_duration,
                                                                });
                                                            }
                                                        }
                                                        Ok::<_, crate::audio::decoder::DecodeError>(decoded)
                                                    }).await {
                                                        Ok(Ok(decoded)) => {
                                                            let st = app_for_decode.state::<AppState>();
                                                            log::info!(
                                                                "[peer] Pre-decoded \"{fid}\" — {} Hz, {} frames — ready for instant playback",
                                                                decoded.sample_rate, decoded.total_frames
                                                            );
                                                            st.decoded_cache.lock().await.insert(fid, decoded);
                                                        }
                                                        Ok(Err(e)) => log::warn!("[peer] Pre-decode of \"{fid}\" failed: {e}"),
                                                        Err(e) => log::warn!("[peer] Pre-decode task of \"{fid}\" failed: {e}"),
                                                    }
                                                });
                                            }
                                        }
                                        // Send FileReceived confirmation back to host.
                                        drop(receiver);
                                        let mut session = s.session.lock().await;
                                        if let Session::Peer(ref mut peer) = &mut *session {
                                            let _ = peer.send(&response).await;
                                        }
                                    }
                                }
                                Message::PlayCommand { file_id, position_samples, .. } => {
                                    log::info!("[peer] Received PlayCommand: file_id={file_id} position_samples={position_samples}");
                                    let s = app_clone.state::<AppState>();

                                    // Try pre-decoded cache first (instant playback).
                                    let pre_decoded = {
                                        let mut dc = s.decoded_cache.lock().await;
                                        dc.remove(&file_id)
                                    };

                                    let decoded = if let Some(d) = pre_decoded {
                                        log::info!("[peer] Using pre-decoded audio for {file_id}");
                                        Some(d)
                                    } else {
                                        // Fall back to decoding from raw cache.
                                        let file_data = {
                                            let cache = s.file_cache.lock().await;
                                            cache.get(&file_id).cloned()
                                        };
                                        if let Some(file_bytes) = file_data {
                                            log::info!("[peer] No pre-decoded audio for {file_id}, decoding {} bytes...", file_bytes.len());
                                            match tokio::task::spawn_blocking(move || {
                                                crate::audio::decoder::decode_mp3(&file_bytes)
                                            }).await {
                                                Ok(Ok(d)) => Some(d),
                                                Ok(Err(e)) => { log::error!("Peer: failed to decode MP3: {e}"); None }
                                                Err(e) => { log::error!("Peer: decode task failed: {e}"); None }
                                            }
                                        } else {
                                            log::warn!("Peer: PlayCommand for unknown file {file_id} — not in cache");
                                            None
                                        }
                                    };

                                    if let Some(decoded) = decoded {
                                        let mut audio = s.audio_output.lock().await;
                                        if audio.is_none() {
                                            match AudioOutput::new() {
                                                Ok(ao) => {
                                                    *s.device_sample_rate.lock() = Some(ao.device_sample_rate);
                                                    *audio = Some(ao);
                                                }
                                                Err(e) => {
                                                    log::error!("Failed to init peer audio: {e}");
                                                    continue;
                                                }
                                            }
                                        }
                                        let ao = audio.as_ref().unwrap();
                                        let vol = *s.volume.lock();
                                        ao.set_volume(vol);
                                        ao.load_track(decoded);
                                        if position_samples > 0 {
                                            ao.seek(position_samples);
                                        }
                                        ao.play_at(std::time::Instant::now());
                                        log::info!("Peer: playing file {file_id}");
                                    }
                                }
                                Message::PauseCommand { position_samples } => {
                                    let s = app_clone.state::<AppState>();
                                    let audio = s.audio_output.lock().await;
                                    if let Some(ref ao) = *audio {
                                        ao.seek(position_samples);
                                        ao.pause();
                                        log::info!("Peer: paused at sample {position_samples}");
                                    }
                                }
                                Message::StopCommand => {
                                    let s = app_clone.state::<AppState>();
                                    let audio = s.audio_output.lock().await;
                                    if let Some(ref ao) = *audio {
                                        ao.stop();
                                        log::info!("Peer: stopped");
                                    }
                                }
                                Message::ResumeCommand { position_samples, .. } => {
                                    let s = app_clone.state::<AppState>();
                                    let audio = s.audio_output.lock().await;
                                    if let Some(ref ao) = *audio {
                                        ao.seek(position_samples);
                                        ao.resume_at(std::time::Instant::now());
                                        log::info!("Peer: resumed at sample {position_samples}");
                                    }
                                }
                                Message::SeekCommand { position_samples, .. } => {
                                    let s = app_clone.state::<AppState>();
                                    let audio = s.audio_output.lock().await;
                                    if let Some(ref ao) = *audio {
                                        ao.seek(position_samples);
                                        ao.resume_at(std::time::Instant::now());
                                        log::info!("Peer: seeked to sample {position_samples}");
                                    }
                                }
                                _ => {}
                            }
                        }
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

    // Stop audio playback first.
    stop_position_ticker(&state).await;
    {
        let audio = state.audio_output.lock().await;
        if let Some(ref ao) = *audio {
            ao.stop();
        }
    }

    // Clear the queue and track data so the next session starts fresh.
    state.queue.lock().await.clear();
    state.track_data.lock().await.clear();
    state.song_requests.lock().await.clear();
    *state.file_receiver.lock().await = FileReceiver::new();
    state.host_queue_state.lock().clear();
    *state.host_current_track.lock() = None;
    *state.host_tcp.lock() = None;

    session.shutdown().await;

    // Notify the frontend.
    let _ = app.emit("playback:state-changed", PlaybackStatePayload {
        state: "stopped".into(),
        file_name: String::new(),
        position_ms: 0,
        duration_ms: 0,
    });
    let _ = app.emit("queue:updated", QueueUpdatedPayload { queue: Vec::new() });
    let _ = app.emit("session:ended", ());
    log::info!("Left session — audio stopped, queue cleared");
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
            drop(session);
            play_current_track(&app, &state).await
        }
        _ => Err("Only the host can control playback.".into()),
    }
}

/// Internal helper: advance the queue if needed, decode the current track,
/// load it into AudioOutput, and start playback.
async fn play_current_track(app: &AppHandle, state: &AppState) -> Result<(), String> {
    let mut queue = state.queue.lock().await;

    // If there is no current track yet (or we already finished), advance.
    let track = match queue.current() {
        Some(t) if t.status == crate::network::messages::QueueItemStatus::Ready => {
            t.clone()
        }
        Some(t) if t.status == crate::network::messages::QueueItemStatus::Playing => {
            // Already playing — treat as resume.
            let file_name = t.file_name.clone();
            let duration_ms = (t.duration_secs * 1000.0) as u64;
            drop(queue);

            let audio = state.audio_output.lock().await;
            if let Some(ref ao) = *audio {
                ao.resume_at(std::time::Instant::now());
            }
            drop(audio);

            emit_playback_state(app, "playing", &file_name, 0, duration_ms);

            // Broadcast ResumeCommand to peers.
            let target_time_ns = now_ns() + 50_000_000; // 50ms safety margin
            broadcast_to_peers(app, &Message::ResumeCommand {
                position_samples: 0,
                target_time_ns,
            });

            return Ok(());
        }
        _ => {
            // Advance to the next ready track.
            let has_tracks = !queue.is_empty();
            match queue.advance() {
                Some(t) => t.clone(),
                None if has_tracks => {
                    // All tracks have been played — restart the queue.
                    queue.restart();
                    match queue.advance() {
                        Some(t) => t.clone(),
                        None => return Err("No tracks ready to play.".into()),
                    }
                }
                None => return Err("No tracks ready to play.".into()),
            }
        }
    };

    let track_id = track.id;
    let file_name = track.file_name.clone();
    let duration_secs = track.duration_secs;
    let duration_ms = (duration_secs * 1000.0) as u64;

    // Mark as Playing.
    queue.mark_playing(track_id);
    let queue_items = queue.get_queue();
    drop(queue);
    emit_queue_update(app, &queue_items);

    // Get the raw file bytes.
    let track_store = state.track_data.lock().await;
    let file_bytes = track_store.get(&track_id)
        .ok_or("Track data not found — file may have been removed.")?
        .clone();
    drop(track_store);

    // Try pre-decoded cache first (instant path).
    let pre_decoded = {
        let mut dc = state.decoded_cache.lock().await;
        dc.remove(&track_id)
    };

    let decoded = if let Some(d) = pre_decoded {
        log::info!("[host] Using pre-decoded audio for {track_id}");
        d
    } else {
        // Fall back to decoding on a blocking thread.
        log::info!("[host] No pre-decoded audio for {track_id}, decoding {} bytes...", file_bytes.len());
        tokio::task::spawn_blocking(move || {
            crate::audio::decoder::decode_mp3(&file_bytes)
        })
        .await
        .map_err(|e| format!("Decode task failed: {e}"))?
        .map_err(|e| format!("Failed to decode MP3: {e}"))?
    };

    // Lazily create AudioOutput if it doesn't exist yet.
    let mut audio = state.audio_output.lock().await;
    if audio.is_none() {
        let ao = AudioOutput::new().map_err(|e| format!("Failed to init audio: {e}"))?;
        *state.device_sample_rate.lock() = Some(ao.device_sample_rate);
        *audio = Some(ao);
    }

    let ao = audio.as_ref().unwrap();

    // Set volume.
    let vol = *state.volume.lock();
    ao.set_volume(vol);

    // Load the track (should be instant if pre-resampled).
    ao.load_track(decoded);

    // Update shared current track state for new peer joins.
    let actual_sample_rate = ao.playback_state().sample_rate.load(std::sync::atomic::Ordering::Acquire);
    drop(audio);

    *state.host_current_track.lock() = Some(CurrentTrack {
        file_id: track_id,
        file_name: file_name.clone(),
        position_samples: 0,
        sample_rate: actual_sample_rate,
        is_playing: true,
    });

    // Broadcast PlayCommand to peers FIRST, before we start local playback.
    // This way the network message is already in flight while we wait.
    let target_time_ns = now_ns() + 50_000_000;
    broadcast_to_peers(app, &Message::PlayCommand {
        file_id: track_id,
        position_samples: 0,
        target_time_ns,
    });

    // Emit state.
    emit_playback_state(app, "playing", &file_name, 0, duration_ms);

    // Introduce an artificial delay so peers have time to receive and start
    // playback.  On a typical LAN this is ~150-200 ms.  We schedule the
    // local playback slightly into the future using play_at.
    const HOST_SYNC_DELAY_MS: u64 = 150;
    let play_instant = std::time::Instant::now() + std::time::Duration::from_millis(HOST_SYNC_DELAY_MS);
    {
        let audio = state.audio_output.lock().await;
        if let Some(ref ao) = *audio {
            ao.play_at(play_instant);
        }
    }

    // Start the position ticker.
    start_position_ticker(app, state, duration_ms).await;

    log::info!("Playing track '{}' ({} ms) (delayed by {} ms)", file_name, duration_ms, HOST_SYNC_DELAY_MS);
    Ok(())
}

#[tauri::command]
pub async fn pause(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            drop(session);
            stop_position_ticker(&state).await;

            let audio = state.audio_output.lock().await;
            let (position_ms, file_name, duration_ms) = if let Some(ref ao) = *audio {
                ao.pause();
                let pos_frames = ao.get_position();
                let sr = ao.playback_state().sample_rate.load(std::sync::atomic::Ordering::Acquire);
                let pos_ms = if sr > 0 { (pos_frames as u64 * 1000) / sr as u64 } else { 0 };
                drop(audio);

                let queue = state.queue.lock().await;
                let (fname, dur) = queue.current()
                    .map(|t| (t.file_name.clone(), (t.duration_secs * 1000.0) as u64))
                    .unwrap_or_default();
                (pos_ms, fname, dur)
            } else {
                drop(audio);
                (0, String::new(), 0)
            };

            emit_playback_state(&app, "paused", &file_name, position_ms, duration_ms);

            // Broadcast PauseCommand to peers.
            let pos_samples = {
                let audio = state.audio_output.lock().await;
                audio.as_ref().map(|ao| ao.get_position()).unwrap_or(0)
            };
            broadcast_to_peers(&app, &Message::PauseCommand {
                position_samples: pos_samples,
            });

            // Update shared current track state.
            {
                let mut ct = state.host_current_track.lock();
                if let Some(ref mut track) = *ct {
                    track.is_playing = false;
                }
            }

            log::info!("Paused playback");
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
            drop(session);
            stop_position_ticker(&state).await;

            let audio = state.audio_output.lock().await;
            if let Some(ref ao) = *audio {
                ao.stop();
            }
            drop(audio);

            emit_playback_state(&app, "stopped", "", 0, 0);

            // Broadcast StopCommand to peers.
            broadcast_to_peers(&app, &Message::StopCommand);

            // Clear shared current track state.
            *state.host_current_track.lock() = None;

            log::info!("Stopped playback");
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
            drop(session);

            let audio = state.audio_output.lock().await;
            if let Some(ref ao) = *audio {
                let sr = ao.playback_state().sample_rate.load(std::sync::atomic::Ordering::Acquire);
                let frame = (position_ms as u64 * sr as u64) / 1000;
                ao.seek(frame);
            }
            drop(audio);

            let queue = state.queue.lock().await;
            let duration_ms = queue.current()
                .map(|t| (t.duration_secs * 1000.0) as u64)
                .unwrap_or(0);
            drop(queue);

            let _ = app.emit(
                "playback:position",
                PlaybackPositionPayload {
                    position_ms,
                    duration_ms,
                },
            );

            // Broadcast SeekCommand to peers.
            let seek_samples = {
                let audio = state.audio_output.lock().await;
                audio.as_ref().map(|ao| ao.get_position()).unwrap_or(0)
            };
            let target_time_ns = now_ns() + 50_000_000;
            broadcast_to_peers(&app, &Message::SeekCommand {
                position_samples: seek_samples,
                target_time_ns,
            });

            log::info!("Seek to {position_ms} ms");
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
            drop(session);

            // Stop current playback.
            stop_position_ticker(&state).await;
            let audio = state.audio_output.lock().await;
            if let Some(ref ao) = *audio {
                ao.stop();
            }
            drop(audio);

            // Advance the queue.
            let mut queue = state.queue.lock().await;
            let has_next = queue.skip().is_some();
            let queue_items = queue.get_queue();
            drop(queue);

            emit_queue_update(&app, &queue_items);

            if has_next {
                log::info!("Skipped to next track — starting playback");
                play_current_track(&app, &state).await?;
            } else {
                log::info!("Skip: no more tracks");
                emit_playback_state(&app, "stopped", "", 0, 0);
                broadcast_to_peers(&app, &Message::StopCommand);
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
        Session::Host(host) => {
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

            // Validate MP3 by trying to decode — keep result for pre-cached playback.
            let decoded = match crate::audio::decoder::decode_mp3(&file_data) {
                Ok(d) => d,
                Err(_e) => return Err("Invalid or corrupted MP3 file.".into()),
            };
            let duration_secs = decoded.duration_secs;

            // Pre-resample to device rate if known, so play is instant.
            let target_rate = *state.device_sample_rate.lock();
            let cached_decoded = if let Some(dev_rate) = target_rate {
                if decoded.sample_rate != dev_rate {
                    log::info!("[add_song] Pre-resampling from {} Hz to {} Hz", decoded.sample_rate, dev_rate);
                    let resampled = AudioOutput::resample(
                        &decoded.samples,
                        decoded.channels,
                        decoded.sample_rate,
                        dev_rate,
                    );
                    let new_frames = resampled.len() as u64 / decoded.channels as u64;
                    let new_duration = new_frames as f64 / dev_rate as f64;
                    crate::audio::decoder::DecodedAudio {
                        samples: resampled,
                        sample_rate: dev_rate,
                        channels: decoded.channels,
                        total_frames: new_frames,
                        duration_secs: new_duration,
                    }
                } else {
                    decoded
                }
            } else {
                decoded
            };

            let mut queue = state.queue.lock().await;
            let track_id = queue.add(file_name.clone(), duration_secs, "host".into());
            queue.mark_ready(track_id);
            let queue_items = queue.get_queue();
            drop(queue);

            // Store the raw file bytes so we can decode & play later.
            state.track_data.lock().await.insert(track_id, file_data.clone());

            // Store pre-decoded (and pre-resampled) audio for instant playback.
            state.decoded_cache.lock().await.insert(track_id, cached_decoded);
            log::info!("[add_song] Pre-cached decoded audio for {track_id}");

            // Transfer the file to all connected peers.
            let peer_ids: Vec<u32> = host.peers.lock().keys().copied().collect();
            log::info!("[add_song] track_id={track_id} file={file_name} size={} peers={:?}", file_data.len(), peer_ids);
            if !peer_ids.is_empty() {
                let tcp_host = host.tcp_host.clone();
                let mut mgr = state.file_transfer_mgr.lock().await;
                if let Err(e) = mgr.start_transfer_with_id(
                    track_id,
                    file_name.clone(),
                    file_data,
                    &peer_ids,
                    &tcp_host,
                ).await {
                    log::warn!("Failed to transfer file to peers: {e}");
                }
            }

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

    // Also apply to the live AudioOutput if it exists.
    let audio = state.audio_output.lock().await;
    if let Some(ref ao) = *audio {
        ao.set_volume(clamped);
    }

    log::info!("Volume set to {clamped:.2}");
    Ok(())
}

// ── Auto-advance ────────────────────────────────────────────────────────────

/// Called when a track finishes to automatically play the next queued track.
async fn auto_advance_to_next(app: AppHandle) {
    let state = app.state::<AppState>();

    // Mark the current track as Played and advance the queue.
    let mut queue = state.queue.lock().await;
    let has_next = queue.skip().is_some();
    let queue_items = queue.get_queue();
    drop(queue);

    emit_queue_update(&app, &queue_items);

    if has_next {
        log::info!("Track finished — auto-advancing to next track");
        if let Err(e) = play_current_track(&app, &state).await {
            log::error!("Auto-advance play failed: {e}");
            emit_error(&app, &format!("Failed to play next track: {e}"));
        }
    } else {
        log::info!("Track finished — no more tracks in queue");
        emit_playback_state(&app, "stopped", "", 0, 0);
        broadcast_to_peers(&app, &Message::StopCommand);
    }
}

/// Register the internal event listener that auto-advances to the next track
/// when the current one finishes.  Call this once during app setup.
pub fn setup_auto_advance_listener(app: &AppHandle) {
    use tauri::Listener;
    let app_clone = app.clone();
    app.listen("internal:auto-advance", move |_| {
        let app = app_clone.clone();
        tauri::async_runtime::spawn(async move {
            auto_advance_to_next(app).await;
        });
    });
}

// ── Playback Position Ticker ────────────────────────────────────────────────

/// Start a background task that emits `playback:position` every 250 ms,
/// reading the real position from AudioOutput.
pub async fn start_position_ticker(
    app: &AppHandle,
    state: &AppState,
    duration_ms: u64,
) {
    // Stop any existing ticker first.
    stop_position_ticker(state).await;

    let app_clone = app.clone();
    // We need a reference to the audio output's PlaybackState (which is Send+Sync).
    let playback_state = {
        let audio = state.audio_output.lock().await;
        audio.as_ref().map(|ao| ao.playback_state())
    };

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(250));

        loop {
            interval.tick().await;

            let position_ms = if let Some(ref ps) = playback_state {
                let pos_samples = ps.position.load(std::sync::atomic::Ordering::Acquire) as u64;
                let channels = ps.channels.load(std::sync::atomic::Ordering::Acquire) as u64;
                let sr = ps.sample_rate.load(std::sync::atomic::Ordering::Acquire) as u64;
                if sr > 0 && channels > 0 {
                    let frames = pos_samples / channels;
                    (frames * 1000) / sr
                } else {
                    0
                }
            } else {
                0
            };

            // Check if the track ended (AudioOutput transitions to Stopped).
            let is_stopped = playback_state.as_ref().map_or(true, |ps| {
                ps.state.load(std::sync::atomic::Ordering::Acquire) == 0 // STATE_STOPPED
            });

            if is_stopped && position_ms > 0 {
                // Track finished — request auto-advance via internal event.
                let _ = app_clone.emit("playback:track-finished", ());
                let _ = app_clone.emit("internal:auto-advance", ());
                break;
            }

            if position_ms > duration_ms {
                let _ = app_clone.emit("playback:track-finished", ());
                let _ = app_clone.emit("internal:auto-advance", ());
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
