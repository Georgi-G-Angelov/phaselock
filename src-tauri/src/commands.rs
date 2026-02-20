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
struct ListenerInfo {
    peer_id: u32,
    display_name: String,
}

#[derive(Clone, Serialize)]
struct ListenersUpdatedPayload {
    host_name: String,
    listeners: Vec<ListenerInfo>,
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

// ── Helper ──────────────────────────────────────────────────────────────────

fn emit_error(app: &AppHandle, msg: &str) {
    let _ = app.emit("error:general", ErrorPayload { message: msg.to_string() });
}

/// Convert a sample position from one sample rate to another.
/// E.g. frame 44100 at 44100 Hz = 1 second = frame 48000 at 48000 Hz.
fn convert_sample_position(position_frames: u64, src_rate: u32, dst_rate: u32) -> u64 {
    if src_rate == dst_rate || src_rate == 0 {
        return position_frames;
    }
    (position_frames as f64 * dst_rate as f64 / src_rate as f64).round() as u64
}

/// Compute how many frames of latency compensation to add on the peer side.
///
/// The peer receives a playback command ~latency ms after the host sent it,
/// so by the time the peer starts playing the host is already `latency` ahead.
/// We skip that many frames forward so both devices stay in sync.
fn latency_compensation_frames(latency_ns: u64, sample_rate: u32) -> u64 {
    // frames = latency_seconds * sample_rate
    (latency_ns as f64 / 1_000_000_000.0 * sample_rate as f64).round() as u64
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

    // Broadcast to peers over TCP with full playback info for catch-up.
    let state = app.state::<AppState>();
    let tcp_host = state.host_tcp.lock().clone();
    if let Some(tcp_host) = tcp_host {
        let ct = state.host_current_track.lock().clone();
        let (file_id, position_samples, sample_rate) = ct
            .map(|t| (t.file_id, t.position_samples, t.sample_rate))
            .unwrap_or((Uuid::nil(), 0, 0));
        let msg = Message::PlaybackStateUpdate {
            state: pstate.to_string(),
            file_name: file_name.to_string(),
            position_ms,
            duration_ms,
            file_id,
            position_samples,
            sample_rate,
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

/// Maximum number of decoded tracks to keep in memory (playing + next N).
const DECODED_CACHE_WINDOW: usize = 5;

/// Check whether a track id falls within the current decoded-cache window.
///
/// Returns `true` when the track is among the currently playing track and
/// the next `DECODED_CACHE_WINDOW - 1` tracks in the queue, meaning it is
/// worth pre-decoding.
async fn is_in_cache_window(state: &AppState, track_id: Uuid) -> bool {
    let queue = state.queue.lock().await;
    queue.upcoming_ids(DECODED_CACHE_WINDOW).contains(&track_id)
}

/// Evict decoded audio entries that fall outside the playback window.
///
/// Keeps only the currently playing track and the next `DECODED_CACHE_WINDOW - 1`
/// tracks in the queue. Must be called after any decoded_cache insertion or
/// queue mutation (advance, reorder, remove).
async fn evict_decoded_cache(state: &AppState) {
    let keep_ids = {
        let queue = state.queue.lock().await;
        queue.upcoming_ids(DECODED_CACHE_WINDOW)
    };

    let mut dc = state.decoded_cache.lock().await;
    let before = dc.len();
    dc.retain(|id, _| keep_ids.contains(id));
    let evicted = before - dc.len();
    if evicted > 0 {
        log::info!(
            "[cache] Evicted {evicted} decoded track(s) outside playback window, keeping {}",
            dc.len()
        );
    }
}

/// Backfill the decoded cache for any tracks inside the playback window
/// that are not yet decoded.
///
/// Spawns background decode (+ optional resample) tasks for each missing
/// track. Called after the window shifts (play, skip, advance).
async fn backfill_decoded_cache(app: &AppHandle, state: &AppState) {
    // Determine which IDs are in the window but not yet in the cache.
    let (window_ids, is_host) = {
        let queue = state.queue.lock().await;
        let ids = queue.upcoming_ids(DECODED_CACHE_WINDOW);
        let session = state.session.lock().await;
        let host = matches!(&*session, Session::Host(_));
        (ids, host)
    };

    let cached_ids: std::collections::HashSet<Uuid> = {
        let dc = state.decoded_cache.lock().await;
        dc.keys().copied().collect()
    };

    let missing: Vec<Uuid> = window_ids
        .into_iter()
        .filter(|id| !cached_ids.contains(id))
        .collect();

    if missing.is_empty() {
        return;
    }

    log::info!(
        "[cache] Backfilling {} track(s) into decoded cache",
        missing.len()
    );

    let target_rate = *state.device_sample_rate.lock();

    for file_id in missing {
        // Get the raw bytes — host keeps them in track_data, peer in file_cache.
        let raw_bytes: Option<Vec<u8>> = if is_host {
            let store = state.track_data.lock().await;
            store.get(&file_id).cloned()
        } else {
            let cache = state.file_cache.lock().await;
            cache.get(&file_id).cloned()
        };

        let Some(bytes) = raw_bytes else {
            log::debug!(
                "[cache] Cannot backfill {file_id}: raw bytes not available yet"
            );
            continue;
        };

        let app_clone = app.clone();
        tokio::spawn(async move {
            log::info!("[cache] Backfill: decoding {file_id}...");
            match tokio::task::spawn_blocking(move || {
                let decoded = crate::audio::decoder::decode_mp3(&bytes)?;
                if let Some(dev_rate) = target_rate {
                    if decoded.sample_rate != dev_rate {
                        log::info!(
                            "[cache] Backfill: resampling {file_id} from {} Hz to {} Hz",
                            decoded.sample_rate,
                            dev_rate
                        );
                        let resampled = AudioOutput::resample(
                            &decoded.samples,
                            decoded.channels,
                            decoded.sample_rate,
                            dev_rate,
                        );
                        let new_frames =
                            resampled.len() as u64 / decoded.channels as u64;
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
            })
            .await
            {
                Ok(Ok(decoded)) => {
                    let st = app_clone.state::<AppState>();
                    // Re-check window: the queue may have changed while we decoded.
                    if is_in_cache_window(&st, file_id).await {
                        log::info!(
                            "[cache] Backfill: cached {file_id} — {} Hz, {} frames",
                            decoded.sample_rate,
                            decoded.total_frames
                        );
                        st.decoded_cache.lock().await.insert(file_id, decoded);
                    } else {
                        log::info!(
                            "[cache] Backfill: {file_id} left window during decode, discarding"
                        );
                    }
                }
                Ok(Err(e)) => {
                    log::warn!("[cache] Backfill: decode of {file_id} failed: {e}")
                }
                Err(e) => {
                    log::warn!("[cache] Backfill: decode task of {file_id} failed: {e}")
                }
            }
        });
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
                                        if let Some(tcp_host) = tcp_host {
                                            // Gather data needed for transfer without holding locks long-term.
                                            let track_store = s.track_data.lock().await;
                                            let queue_lock = s.queue.lock().await;
                                            let mut files_to_transfer: Vec<(Uuid, String, Vec<u8>)> = Vec::new();
                                            for file_id in &missing {
                                                if let Some(data) = track_store.get(file_id) {
                                                    let file_name = queue_lock.get_queue().iter()
                                                        .find(|q| q.id == *file_id)
                                                        .map(|q| q.file_name.clone())
                                                        .unwrap_or_default();
                                                    files_to_transfer.push((*file_id, file_name, data.clone()));
                                                }
                                            }
                                            drop(queue_lock);
                                            drop(track_store);

                                            // Spawn transfer in background so event loop stays responsive.
                                            let app_for_transfer = app_clone.clone();
                                            tokio::spawn(async move {
                                                let s = app_for_transfer.state::<AppState>();
                                                let mut mgr = s.file_transfer_mgr.lock().await;
                                                for (file_id, file_name, data) in files_to_transfer {
                                                    if let Err(e) = mgr.start_transfer_with_id(
                                                        file_id,
                                                        file_name.clone(),
                                                        data,
                                                        &[peer_id],
                                                        &tcp_host,
                                                    ).await {
                                                        log::warn!("Failed to transfer \"{file_name}\" to peer {peer_id}: {e}");
                                                    }
                                                }
                                            });
                                        }
                                    } else {
                                        log::info!("Peer {peer_id} already has all files");
                                    }
                                }
                                Message::SongRequest { file_name, file_size } => {
                                    log::info!("[host] Received SongRequest from peer {peer_id}: \"{file_name}\" ({file_size} bytes)");
                                    let s = app_clone.state::<AppState>();

                                    // Look up peer display name.
                                    let peer_name = {
                                        let session = s.session.lock().await;
                                        if let Session::Host(ref host) = *session {
                                            host.peers.lock()
                                                .get(&peer_id)
                                                .map(|p| p.display_name.clone())
                                                .unwrap_or_else(|| format!("Peer {peer_id}"))
                                        } else {
                                            format!("Peer {peer_id}")
                                        }
                                    };

                                    let mut requests = s.song_requests.lock().await;
                                    match requests.receive_request(peer_id, peer_name.clone(), file_name.clone(), file_size) {
                                        Ok(pending) => {
                                            #[derive(Clone, serde::Serialize)]
                                            struct SongRequestIncoming {
                                                request_id: String,
                                                peer_name: String,
                                                file_name: String,
                                                file_size: u64,
                                            }
                                            let _ = app_clone.emit(
                                                "request:incoming",
                                                SongRequestIncoming {
                                                    request_id: pending.request_id.to_string(),
                                                    peer_name,
                                                    file_name,
                                                    file_size,
                                                },
                                            );
                                        }
                                        Err(reason) => {
                                            log::warn!("[host] Auto-rejected song request from peer {peer_id}: {reason:?}");
                                        }
                                    }
                                }
                                Message::SongUploadChunk { request_id, offset, data } => {
                                    let s = app_clone.state::<AppState>();
                                    let mut requests = s.song_requests.lock().await;
                                    if !requests.receive_chunk(request_id, offset, &data) {
                                        log::warn!("[host] Rejected upload chunk for {request_id}");
                                    }
                                }
                                Message::SongUploadComplete { request_id, sha256 } => {
                                    log::info!("[host] Upload complete for request {request_id}, verifying hash...");
                                    let s = app_clone.state::<AppState>();
                                    let mut requests = s.song_requests.lock().await;
                                    match requests.complete_upload(request_id, sha256) {
                                        Some(crate::transfer::song_request::UploadResult::Success {
                                            file_name, file_data, ..
                                        }) => {
                                            drop(requests);
                                            log::info!("[host] Song upload verified: \"{file_name}\" ({} bytes) — processing in background", file_data.len());

                                            // Spawn entire decode→resample→queue→transfer pipeline
                                            // in background so the host event loop stays responsive.
                                            let app_for_upload = app_clone.clone();
                                            tokio::spawn(async move {
                                                let s = app_for_upload.state::<AppState>();

                                                // Validate MP3 by decoding.
                                                let file_data_clone = file_data.clone();
                                                let decoded = match tokio::task::spawn_blocking(move || {
                                                    crate::audio::decoder::decode_mp3(&file_data_clone)
                                                }).await {
                                                    Ok(Ok(d)) => d,
                                                    Ok(Err(e)) => {
                                                        log::error!("[host] Uploaded file is not a valid MP3: {e}");
                                                        return;
                                                    }
                                                    Err(e) => {
                                                        log::error!("[host] Decode task failed: {e}");
                                                        return;
                                                    }
                                                };
                                                let duration_secs = decoded.duration_secs;

                                                // Add to queue first (before cache decision) so
                                                // we can check whether this track is in the window.
                                                let track_id = request_id;
                                                let mut queue = s.queue.lock().await;
                                                queue.add_with_id(track_id, file_name.clone(), duration_secs, format!("peer {peer_id}"));
                                                queue.mark_ready(track_id);
                                                let queue_items = queue.get_queue();
                                                drop(queue);

                                                // Store raw file data.
                                                s.track_data.lock().await.insert(track_id, file_data.clone());

                                                // Only pre-resample and cache if track is within the playback window.
                                                if is_in_cache_window(&s, track_id).await {
                                                    // Pre-resample if needed (on blocking thread).
                                                    let target_rate = *s.device_sample_rate.lock();
                                                    let cached_decoded = if let Some(dev_rate) = target_rate {
                                                        if decoded.sample_rate != dev_rate {
                                                            log::info!("[host] Pre-resampling uploaded track from {} Hz to {} Hz", decoded.sample_rate, dev_rate);
                                                            match tokio::task::spawn_blocking(move || {
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
                                                            }).await {
                                                                Ok(d) => d,
                                                                Err(e) => {
                                                                    log::error!("[host] Resample task failed: {e}");
                                                                    // Continue without caching — playback will decode on demand.
                                                                    emit_queue_update(&app_for_upload, &queue_items);
                                                                    return;
                                                                }
                                                            }
                                                        } else {
                                                            decoded
                                                        }
                                                    } else {
                                                        decoded
                                                    };

                                                    s.decoded_cache.lock().await.insert(track_id, cached_decoded);
                                                    log::info!("[host] Pre-cached decoded audio for uploaded track {track_id}");
                                                    evict_decoded_cache(&s).await;
                                                } else {
                                                    log::info!("[host] Uploaded track {track_id} outside cache window, skipping pre-cache");
                                                }

                                                // Transfer the file to other peers (exclude the uploader — they already have it).
                                                let tcp_host = s.host_tcp.lock().clone();
                                                if let Some(tcp_host) = tcp_host {
                                                    let session = s.session.lock().await;
                                                    if let Session::Host(ref host) = *session {
                                                        let other_peer_ids: Vec<u32> = host.peers.lock()
                                                            .keys()
                                                            .copied()
                                                            .filter(|&id| id != peer_id)
                                                            .collect();
                                                        drop(session);
                                                        if !other_peer_ids.is_empty() {
                                                            let mut mgr = s.file_transfer_mgr.lock().await;
                                                            if let Err(e) = mgr.start_transfer_with_id(
                                                                track_id,
                                                                file_name.clone(),
                                                                file_data,
                                                                &other_peer_ids,
                                                                &tcp_host,
                                                            ).await {
                                                                log::warn!("[host] Failed to transfer uploaded file to peers: {e}");
                                                            }
                                                        }
                                                    }
                                                }

                                                emit_queue_update(&app_for_upload, &queue_items);
                                                log::info!("[host] Added uploaded song \"{}\" to queue ({})", file_name, track_id);
                                            });
                                        }
                                        Some(crate::transfer::song_request::UploadResult::HashMismatch { request_id }) => {
                                            drop(requests);
                                            log::error!("[host] Upload hash mismatch for request {request_id}");
                                        }
                                        None => {
                                            log::warn!("[host] No active upload found for request {request_id}");
                                        }
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

            // Start the latency-logging ticker.
            {
                let app_for_latency = app.clone();
                let handle = tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                    loop {
                        interval.tick().await;
                        let s = app_for_latency.state::<AppState>();
                        let session = s.session.lock().await;
                        if let Session::Host(ref host) = *session {
                            if let Some(ref udp) = host.udp_host {
                                let latencies = udp.tracker.lock().get_all_latencies();
                                if !latencies.is_empty() {
                                    for (peer_id, lat_ns) in &latencies {
                                        let latency_ms = *lat_ns as f64 / 1_000_000.0;
                                        log::info!("[latency] peer {peer_id}: {:.2} ms one-way", latency_ms);
                                        let _ = app_for_latency.emit(
                                            "sync:latency-updated",
                                            SyncLatencyPayload { peer_id: *peer_id, latency_ms },
                                        );
                                    }
                                }
                                // Update the session info for UDP broadcast.
                                let listeners: Vec<(u32, String)> = host.peers.lock()
                                    .values()
                                    .map(|p| (p.peer_id, p.display_name.clone()))
                                    .collect();
                                *udp.session_info.lock() = (host.host_display_name.clone(), listeners);
                            }
                        }
                    }
                });
                *state.latency_ticker.lock().await = Some(handle);
            }

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

                                    // Sync local queue state so decoded-cache eviction
                                    // knows which tracks are upcoming.
                                    let s = app_clone.state::<AppState>();
                                    {
                                        let mut q = s.queue.lock().await;
                                        q.replace_all(queue.clone());
                                    }
                                    evict_decoded_cache(&s).await;
                                    backfill_decoded_cache(&app_clone, &s).await;

                                    let _ = app_clone.emit(
                                        "queue:updated",
                                        QueueUpdatedPayload { queue },
                                    );
                                }
                                Message::PlaybackStateUpdate { state, file_name, position_ms, duration_ms, file_id, position_samples, sample_rate: host_sr } => {
                                    log::info!("[peer] Received PlaybackStateUpdate: state={state} file={file_name} pos={position_ms} dur={duration_ms} file_id={file_id} pos_samples={position_samples} host_sr={host_sr}");

                                    // ── Peer catch-up: if host is playing but we are not, start playback ──
                                    if state == "playing" {
                                        let received_at = std::time::Instant::now();
                                        let s = app_clone.state::<AppState>();

                                        // Check current peer playback state and which file is loaded.
                                        let (peer_state, peer_file_id) = {
                                            let audio = s.audio_output.lock().await;
                                            let st = if let Some(ref ao) = *audio {
                                                ao.get_state()
                                            } else {
                                                crate::audio::playback::PlaybackStateEnum::Stopped
                                            };
                                            let fid = *s.peer_current_file_id.lock();
                                            (st, fid)
                                        };

                                        use crate::audio::playback::PlaybackStateEnum;

                                        // Determine if we need to catch up.
                                        let same_file = peer_file_id == Some(file_id);
                                        let needs_full_catchup = peer_state == PlaybackStateEnum::Stopped
                                            || (peer_state == PlaybackStateEnum::Playing && !same_file)
                                            || (peer_state == PlaybackStateEnum::Waiting && !same_file);

                                        if peer_state == PlaybackStateEnum::Paused && same_file {
                                            // ── Fast path: track already loaded, just seek & resume ──
                                            log::info!("[peer] Catch-up: peer is paused, resuming at host position for {file_id}");

                                            let latency_ns = {
                                                let session = s.session.lock().await;
                                                if let Session::Peer(ref peer) = *session {
                                                    peer.clock_sync.lock().current_latency_ns
                                                } else { 0 }
                                            };

                                            let audio = s.audio_output.lock().await;
                                            if let Some(ref ao) = *audio {
                                                let peer_sr = ao.device_sample_rate;
                                                let mut adjusted = convert_sample_position(position_samples, host_sr, peer_sr);
                                                let processing_ns = received_at.elapsed().as_nanos() as u64;
                                                let total_delay_ns = latency_ns + processing_ns;
                                                let comp = latency_compensation_frames(total_delay_ns, peer_sr);
                                                adjusted += comp;

                                                ao.seek(adjusted);
                                                ao.resume_at(std::time::Instant::now());
                                                drop(audio);
                                                start_peer_position_ticker(&app_clone, &s).await;
                                                log::info!(
                                                    "[peer] Catch-up: resumed {file_id} at frame {adjusted} (comp +{comp}: {:.2} ms network + {:.2} ms processing)",
                                                    latency_ns as f64 / 1_000_000.0,
                                                    processing_ns as f64 / 1_000_000.0,
                                                );
                                            }
                                        } else if needs_full_catchup {
                                            // ── Full path: need to decode and load the track ──
                                            if !same_file && (peer_state == PlaybackStateEnum::Playing || peer_state == PlaybackStateEnum::Waiting) {
                                                log::info!("[peer] Catch-up: peer is playing a different file — switching to {file_id}");
                                            }
                                            log::info!("[peer] Catch-up: host is playing but peer needs new track — attempting to start playback for {file_id}");

                                            // Try pre-decoded cache first (instant).
                                            let pre_decoded = {
                                                let mut dc = s.decoded_cache.lock().await;
                                                dc.remove(&file_id)
                                            };

                                            if let Some(decoded) = pre_decoded {
                                                // Fast path: pre-decoded cache hit — handle inline.
                                                log::info!("[peer] Catch-up: using pre-decoded audio for {file_id}");

                                                let latency_ns = {
                                                    let session = s.session.lock().await;
                                                    if let Session::Peer(ref peer) = *session {
                                                        peer.clock_sync.lock().current_latency_ns
                                                    } else { 0 }
                                                };

                                                let mut audio = s.audio_output.lock().await;
                                                if audio.is_none() {
                                                    match AudioOutput::new() {
                                                        Ok(ao) => {
                                                            *s.device_sample_rate.lock() = Some(ao.device_sample_rate);
                                                            *audio = Some(ao);
                                                        }
                                                        Err(e) => {
                                                            log::error!("[peer] Catch-up: failed to init audio: {e}");
                                                            let _ = app_clone.emit(
                                                                "playback:state-changed",
                                                                PlaybackStatePayload { state, file_name, position_ms, duration_ms },
                                                            );
                                                            continue;
                                                        }
                                                    }
                                                }
                                                let ao = audio.as_ref().unwrap();
                                                let vol = *s.volume.lock();
                                                ao.set_volume(vol);
                                                ao.load_track(decoded);

                                                let peer_sr = ao.device_sample_rate;
                                                let mut adjusted = convert_sample_position(position_samples, host_sr, peer_sr);
                                                let processing_ns = received_at.elapsed().as_nanos() as u64;
                                                let total_delay_ns = latency_ns + processing_ns;
                                                let comp = latency_compensation_frames(total_delay_ns, peer_sr);
                                                adjusted += comp;

                                                ao.play_at(std::time::Instant::now());
                                                if adjusted > 0 {
                                                    ao.seek(adjusted);
                                                }
                                                *s.peer_current_file_id.lock() = Some(file_id);
                                                drop(audio);
                                                start_peer_position_ticker(&app_clone, &s).await;
                                                log::info!(
                                                    "[peer] Catch-up: playing {file_id} at frame {adjusted} (comp +{comp}: {:.2} ms network + {:.2} ms processing)",
                                                    latency_ns as f64 / 1_000_000.0,
                                                    processing_ns as f64 / 1_000_000.0,
                                                );
                                            } else {
                                                // Slow path: decode needed — spawn in background
                                                // so the event loop stays responsive to commands.
                                                let file_data = {
                                                    let cache = s.file_cache.lock().await;
                                                    cache.get(&file_id).cloned()
                                                };
                                                if let Some(file_bytes) = file_data {
                                                    let app_for_catchup = app_clone.clone();
                                                    tokio::spawn(async move {
                                                        let s = app_for_catchup.state::<AppState>();
                                                        log::info!("[peer] Catch-up (bg): decoding {} bytes for {file_id}...", file_bytes.len());
                                                        let decoded = match tokio::task::spawn_blocking(move || {
                                                            crate::audio::decoder::decode_mp3(&file_bytes)
                                                        }).await {
                                                            Ok(Ok(d)) => d,
                                                            Ok(Err(e)) => { log::error!("[peer] Catch-up (bg): failed to decode MP3: {e}"); return; }
                                                            Err(e) => { log::error!("[peer] Catch-up (bg): decode task failed: {e}"); return; }
                                                        };

                                                        let latency_ns = {
                                                            let session = s.session.lock().await;
                                                            if let Session::Peer(ref peer) = *session {
                                                                peer.clock_sync.lock().current_latency_ns
                                                            } else { 0 }
                                                        };

                                                        let mut audio = s.audio_output.lock().await;
                                                        if audio.is_none() {
                                                            match AudioOutput::new() {
                                                                Ok(ao) => {
                                                                    *s.device_sample_rate.lock() = Some(ao.device_sample_rate);
                                                                    *audio = Some(ao);
                                                                }
                                                                Err(e) => {
                                                                    log::error!("[peer] Catch-up (bg): failed to init audio: {e}");
                                                                    return;
                                                                }
                                                            }
                                                        }
                                                        let ao = audio.as_ref().unwrap();
                                                        let vol = *s.volume.lock();
                                                        ao.set_volume(vol);
                                                        ao.load_track(decoded);

                                                        let peer_sr = ao.device_sample_rate;
                                                        let mut adjusted = convert_sample_position(position_samples, host_sr, peer_sr);
                                                        let processing_ns = received_at.elapsed().as_nanos() as u64;
                                                        let total_delay_ns = latency_ns + processing_ns;
                                                        let comp = latency_compensation_frames(total_delay_ns, peer_sr);
                                                        adjusted += comp;

                                                        ao.play_at(std::time::Instant::now());
                                                        if adjusted > 0 {
                                                            ao.seek(adjusted);
                                                        }
                                                        *s.peer_current_file_id.lock() = Some(file_id);
                                                        drop(audio);
                                                        start_peer_position_ticker(&app_for_catchup, &s).await;
                                                        log::info!(
                                                            "[peer] Catch-up (bg): playing {file_id} at frame {adjusted} (comp +{comp}: {:.2} ms network + {:.2} ms processing)",
                                                            latency_ns as f64 / 1_000_000.0,
                                                            processing_ns as f64 / 1_000_000.0,
                                                        );
                                                    });
                                                } else {
                                                    log::warn!("[peer] Catch-up: file {file_id} not in cache yet (still transferring?)");
                                                }
                                            }
                                        }
                                        // else: Playing/Waiting the same file — already in the right state.
                                    }

                                    // Always emit the UI event.
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

                                                // Pre-decode (and pre-resample) in background so PlayCommand is instant,
                                                // but only if this track is within the cache window.
                                                let fid = *file_id;
                                                if is_in_cache_window(&s, fid).await {
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
                                                                evict_decoded_cache(&st).await;
                                                            }
                                                            Ok(Err(e)) => log::warn!("[peer] Pre-decode of \"{fid}\" failed: {e}"),
                                                            Err(e) => log::warn!("[peer] Pre-decode task of \"{fid}\" failed: {e}"),
                                                        }
                                                    });
                                                } else {
                                                    log::info!("[peer] File \"{fid}\" outside cache window, skipping pre-decode");
                                                }
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
                                Message::PlayCommand { file_id, position_samples, sample_rate: host_sr, .. } => {
                                    let received_at = std::time::Instant::now();
                                    log::info!("[peer] Received PlayCommand: file_id={file_id} position_samples={position_samples} host_sr={host_sr}");
                                    let s = app_clone.state::<AppState>();

                                    // Try pre-decoded cache first (instant playback).
                                    let pre_decoded = {
                                        let mut dc = s.decoded_cache.lock().await;
                                        dc.remove(&file_id)
                                    };

                                    if let Some(decoded) = pre_decoded {
                                        // Fast path: pre-decoded cache hit — handle inline.
                                        log::info!("[peer] Using pre-decoded audio for {file_id}");

                                        let latency_ns = {
                                            let session = s.session.lock().await;
                                            if let Session::Peer(ref peer) = *session {
                                                peer.clock_sync.lock().current_latency_ns
                                            } else { 0 }
                                        };

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

                                        let peer_sr = ao.device_sample_rate;
                                        let mut adjusted = convert_sample_position(position_samples, host_sr, peer_sr);
                                        let processing_ns = received_at.elapsed().as_nanos() as u64;
                                        let total_delay_ns = latency_ns + processing_ns;
                                        let comp = latency_compensation_frames(total_delay_ns, peer_sr);
                                        adjusted += comp;

                                        ao.play_at(std::time::Instant::now());
                                        if adjusted > 0 {
                                            ao.seek(adjusted);
                                        }
                                        *s.peer_current_file_id.lock() = Some(file_id);
                                        drop(audio);
                                        start_peer_position_ticker(&app_clone, &s).await;
                                        log::info!(
                                            "Peer: playing file {file_id} at frame {adjusted} (comp +{comp} frames: {:.2} ms network + {:.2} ms processing)",
                                            latency_ns as f64 / 1_000_000.0,
                                            processing_ns as f64 / 1_000_000.0,
                                        );
                                    } else {
                                        // Slow path: need to decode from raw file cache.
                                        let file_data = {
                                            let cache = s.file_cache.lock().await;
                                            cache.get(&file_id).cloned()
                                        };
                                        if let Some(file_bytes) = file_data {
                                            // Spawn decode+play in background so event loop
                                            // stays responsive to pause/seek commands.
                                            let app_for_play = app_clone.clone();
                                            tokio::spawn(async move {
                                                let s = app_for_play.state::<AppState>();
                                                log::info!("[peer] PlayCommand (bg): decoding {} bytes for {file_id}...", file_bytes.len());
                                                let decoded = match tokio::task::spawn_blocking(move || {
                                                    crate::audio::decoder::decode_mp3(&file_bytes)
                                                }).await {
                                                    Ok(Ok(d)) => d,
                                                    Ok(Err(e)) => { log::error!("Peer (bg): failed to decode MP3: {e}"); return; }
                                                    Err(e) => { log::error!("Peer (bg): decode task failed: {e}"); return; }
                                                };

                                                let latency_ns = {
                                                    let session = s.session.lock().await;
                                                    if let Session::Peer(ref peer) = *session {
                                                        peer.clock_sync.lock().current_latency_ns
                                                    } else { 0 }
                                                };

                                                let mut audio = s.audio_output.lock().await;
                                                if audio.is_none() {
                                                    match AudioOutput::new() {
                                                        Ok(ao) => {
                                                            *s.device_sample_rate.lock() = Some(ao.device_sample_rate);
                                                            *audio = Some(ao);
                                                        }
                                                        Err(e) => {
                                                            log::error!("Peer (bg): failed to init audio: {e}");
                                                            return;
                                                        }
                                                    }
                                                }
                                                let ao = audio.as_ref().unwrap();
                                                let vol = *s.volume.lock();
                                                ao.set_volume(vol);
                                                ao.load_track(decoded);

                                                let peer_sr = ao.device_sample_rate;
                                                let mut adjusted = convert_sample_position(position_samples, host_sr, peer_sr);
                                                let processing_ns = received_at.elapsed().as_nanos() as u64;
                                                let total_delay_ns = latency_ns + processing_ns;
                                                let comp = latency_compensation_frames(total_delay_ns, peer_sr);
                                                adjusted += comp;

                                                ao.play_at(std::time::Instant::now());
                                                if adjusted > 0 {
                                                    ao.seek(adjusted);
                                                }
                                                *s.peer_current_file_id.lock() = Some(file_id);
                                                drop(audio);
                                                start_peer_position_ticker(&app_for_play, &s).await;
                                                log::info!(
                                                    "Peer (bg): playing file {file_id} at frame {adjusted} (comp +{comp} frames: {:.2} ms network + {:.2} ms processing)",
                                                    latency_ns as f64 / 1_000_000.0,
                                                    processing_ns as f64 / 1_000_000.0,
                                                );
                                            });
                                        } else {
                                            // File not in cache — stop current playback so peer
                                            // doesn't keep playing a different song than the host.
                                            let audio = s.audio_output.lock().await;
                                            if let Some(ref ao) = *audio {
                                                ao.stop();
                                            }
                                            *s.peer_current_file_id.lock() = None;
                                            log::warn!("Peer: stopped playback — PlayCommand for {file_id} but file not available");
                                        }
                                    }
                                }
                                Message::PauseCommand { position_samples, sample_rate: host_sr } => {
                                    let received_at = std::time::Instant::now();
                                    let s = app_clone.state::<AppState>();
                                    stop_position_ticker(&s).await;
                                    let latency_ns = {
                                        let session = s.session.lock().await;
                                        if let Session::Peer(ref peer) = *session {
                                            peer.clock_sync.lock().current_latency_ns
                                        } else { 0 }
                                    };
                                    let audio = s.audio_output.lock().await;
                                    if let Some(ref ao) = *audio {
                                        let peer_sr = ao.device_sample_rate;
                                        let mut adjusted = convert_sample_position(position_samples, host_sr, peer_sr);
                                        let processing_ns = received_at.elapsed().as_nanos() as u64;
                                        let total_delay_ns = latency_ns + processing_ns;
                                        let comp = latency_compensation_frames(total_delay_ns, peer_sr);
                                        adjusted += comp;
                                        ao.seek(adjusted);
                                        ao.pause();
                                        log::info!(
                                            "Peer: paused at frame {adjusted} (comp +{comp}: {:.2} ms network + {:.2} ms processing)",
                                            latency_ns as f64 / 1_000_000.0,
                                            processing_ns as f64 / 1_000_000.0,
                                        );
                                    }
                                }
                                Message::StopCommand => {
                                    let s = app_clone.state::<AppState>();
                                    stop_position_ticker(&s).await;
                                    let audio = s.audio_output.lock().await;
                                    if let Some(ref ao) = *audio {
                                        ao.stop();
                                        *s.peer_current_file_id.lock() = None;
                                        log::info!("Peer: stopped");
                                    }
                                }
                                Message::ResumeCommand { position_samples, sample_rate: host_sr, .. } => {
                                    let received_at = std::time::Instant::now();
                                    let s = app_clone.state::<AppState>();
                                    let latency_ns = {
                                        let session = s.session.lock().await;
                                        if let Session::Peer(ref peer) = *session {
                                            peer.clock_sync.lock().current_latency_ns
                                        } else { 0 }
                                    };
                                    let audio = s.audio_output.lock().await;
                                    if let Some(ref ao) = *audio {
                                        let peer_sr = ao.device_sample_rate;
                                        let mut adjusted = convert_sample_position(position_samples, host_sr, peer_sr);
                                        let processing_ns = received_at.elapsed().as_nanos() as u64;
                                        let total_delay_ns = latency_ns + processing_ns;
                                        let comp = latency_compensation_frames(total_delay_ns, peer_sr);
                                        adjusted += comp;

                                        ao.seek(adjusted);
                                        ao.resume_at(std::time::Instant::now());
                                        drop(audio);
                                        start_peer_position_ticker(&app_clone, &s).await;
                                        log::info!(
                                            "Peer: resumed at frame {adjusted} (comp +{comp}: {:.2} ms network + {:.2} ms processing)",
                                            latency_ns as f64 / 1_000_000.0,
                                            processing_ns as f64 / 1_000_000.0,
                                        );
                                    }
                                }
                                Message::SeekCommand { position_samples, sample_rate: host_sr, .. } => {
                                    let received_at = std::time::Instant::now();
                                    let s = app_clone.state::<AppState>();
                                    let latency_ns = {
                                        let session = s.session.lock().await;
                                        if let Session::Peer(ref peer) = *session {
                                            peer.clock_sync.lock().current_latency_ns
                                        } else { 0 }
                                    };
                                    let audio = s.audio_output.lock().await;
                                    if let Some(ref ao) = *audio {
                                        let peer_sr = ao.device_sample_rate;
                                        let mut adjusted = convert_sample_position(position_samples, host_sr, peer_sr);
                                        let processing_ns = received_at.elapsed().as_nanos() as u64;
                                        let total_delay_ns = latency_ns + processing_ns;
                                        let comp = latency_compensation_frames(total_delay_ns, peer_sr);
                                        adjusted += comp;

                                        ao.seek(adjusted);
                                        ao.resume_at(std::time::Instant::now());
                                        drop(audio);
                                        start_peer_position_ticker(&app_clone, &s).await;
                                        log::info!(
                                            "Peer: seeked to frame {adjusted} (comp +{comp}: {:.2} ms network + {:.2} ms processing)",
                                            latency_ns as f64 / 1_000_000.0,
                                            processing_ns as f64 / 1_000_000.0,
                                        );
                                    }
                                }
                                Message::SongRequestAccepted { request_id, file_name } => {
                                    log::info!("[peer] Song request {request_id} accepted — uploading \"{file_name}\"");
                                    let s = app_clone.state::<AppState>();

                                    // Look up the local file path we stored when sending the request.
                                    let file_path = s.pending_request_paths.lock().remove(&file_name);

                                    if let Some(path) = file_path {
                                        // Spawn upload as a background task so we don't block the
                                        // event loop (which needs to process PlayCommand etc.).
                                        let app_bg = app_clone.clone();
                                        tokio::spawn(async move {
                                            match tokio::fs::read(&path).await {
                                                Ok(file_data) => {
                                                    let sha256 = crate::transfer::file_transfer::compute_sha256(&file_data);

                                                    // Upload in chunks.
                                                    let chunk_size = crate::transfer::song_request::UPLOAD_CHUNK_SIZE;
                                                    let mut offset: u64 = 0;
                                                    let s = app_bg.state::<AppState>();
                                                    for chunk in file_data.chunks(chunk_size) {
                                                        let msg = Message::SongUploadChunk {
                                                            request_id,
                                                            offset,
                                                            data: chunk.to_vec(),
                                                        };
                                                        let mut session = s.session.lock().await;
                                                        if let Session::Peer(ref mut peer) = *session {
                                                            if let Err(e) = peer.send(&msg).await {
                                                                log::error!("[peer] Failed to send upload chunk: {e}");
                                                                return;
                                                            }
                                                        }
                                                        drop(session);
                                                        offset += chunk.len() as u64;
                                                        // Yield briefly to let other tasks (playback, heartbeats) run.
                                                        tokio::task::yield_now().await;
                                                    }

                                                    // Send completion with hash.
                                                    let msg = Message::SongUploadComplete { request_id, sha256 };
                                                    let mut session = s.session.lock().await;
                                                    if let Session::Peer(ref mut peer) = *session {
                                                        let _ = peer.send(&msg).await;
                                                    }
                                                    drop(session);

                                                    // Cache the file locally under request_id (= track_id).
                                                    // The host uses request_id as the file_id in the queue,
                                                    // so both sides agree on the ID without coordination.
                                                    let file_len = file_data.len();
                                                    {
                                                        let mut cache = s.file_cache.lock().await;
                                                        cache.insert(request_id, file_data.clone());
                                                    }
                                                    log::info!("[peer] Cached own upload under {request_id} ({file_len} bytes)");

                                                    // Pre-decode (and pre-resample) in background for instant playback,
                                                    // but only if this track is within the cache window.
                                                    let app_for_decode = app_bg.clone();
                                                    if is_in_cache_window(&s, request_id).await {
                                                        let target_rate = *s.device_sample_rate.lock();
                                                        tokio::spawn(async move {
                                                            log::info!("[peer] Pre-decoding own upload {request_id}...");
                                                            match tokio::task::spawn_blocking(move || {
                                                                let decoded = crate::audio::decoder::decode_mp3(&file_data)?;
                                                                if let Some(dev_rate) = target_rate {
                                                                    if decoded.sample_rate != dev_rate {
                                                                        log::info!(
                                                                            "[peer] Pre-resampling own upload from {} Hz to {} Hz",
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
                                                                        "[peer] Pre-decoded own upload {request_id} — {} Hz, {} frames",
                                                                        decoded.sample_rate, decoded.total_frames
                                                                    );
                                                                    st.decoded_cache.lock().await.insert(request_id, decoded);
                                                                    evict_decoded_cache(&st).await;
                                                                }
                                                                Ok(Err(e)) => log::warn!("[peer] Pre-decode of own upload {request_id} failed: {e}"),
                                                                Err(e) => log::warn!("[peer] Pre-decode task of own upload {request_id} failed: {e}"),
                                                            }
                                                        });
                                                    } else {
                                                        log::info!("[peer] Own upload {request_id} outside cache window, skipping pre-decode");
                                                    }

                                                    log::info!("[peer] Upload complete for request {request_id} ({file_len} bytes)");
                                                }
                                                Err(e) => {
                                                    log::error!("[peer] Failed to read file \"{path}\": {e}");
                                                }
                                            }
                                        });
                                    } else {
                                        log::warn!("[peer] No stored file path for accepted request \"{file_name}\"");
                                    }
                                }
                                Message::SongRequestRejected { request_id } => {
                                    log::info!("[peer] Song request {request_id} was rejected by host");
                                    // Clean up — we don't know the file_name from this message,
                                    // but it's fine to leave it; it'll be cleaned on session end.
                                }
                                _ => {}
                            }
                        }
                    }
                }
            });

            // Start the latency-logging ticker.
            {
                let app_for_latency = app.clone();
                let handle = tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                    loop {
                        interval.tick().await;
                        let s = app_for_latency.state::<AppState>();
                        let session = s.session.lock().await;
                        if let Session::Peer(ref peer) = *session {
                            let cs = peer.clock_sync.lock();
                            if cs.measurement_count() > 0 {
                                log::info!(
                                    "[latency] host: {:.2} ms one-way, offset: {:.2} ms ({} measurements)",
                                    cs.current_latency_ns as f64 / 1_000_000.0,
                                    cs.current_offset_ns as f64 / 1_000_000.0,
                                    cs.measurement_count(),
                                );
                            }
                            drop(cs);

                            // Emit session listeners update if available from UDP.
                            if let Some(ref udp) = peer.udp_peer {
                                if let Some((host_name, raw_listeners)) = udp.session_info.lock().clone() {
                                    let listeners: Vec<ListenerInfo> = raw_listeners.into_iter()
                                        .map(|(pid, name)| ListenerInfo { peer_id: pid, display_name: name })
                                        .collect();
                                    let _ = app_for_latency.emit(
                                        "session:listeners-updated",
                                        ListenersUpdatedPayload { host_name, listeners },
                                    );
                                }
                            }
                        }
                    }
                });
                *state.latency_ticker.lock().await = Some(handle);
            }

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

    // Stop latency ticker and audio playback.
    if let Some(h) = state.latency_ticker.lock().await.take() {
        h.abort();
    }
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
            let (current_pos, sr) = if let Some(ref ao) = *audio {
                let pos = ao.get_position();
                let rate = ao.device_sample_rate;
                ao.resume_at(std::time::Instant::now());
                (pos, rate)
            } else {
                (0, 44100)
            };
            drop(audio);

            let position_ms = if sr > 0 { (current_pos as u64 * 1000) / sr as u64 } else { 0 };
            emit_playback_state(app, "playing", &file_name, position_ms, duration_ms);

            // Restart the position ticker so the progress bar keeps updating.
            start_position_ticker(app, state, duration_ms).await;

            // Broadcast ResumeCommand to peers with actual paused position.
            let target_time_ns = now_ns() + 50_000_000; // 50ms safety margin
            broadcast_to_peers(app, &Message::ResumeCommand {
                position_samples: current_pos,
                target_time_ns,
                sample_rate: sr,
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
        sample_rate: actual_sample_rate,
    });

    // Emit state.
    emit_playback_state(app, "playing", &file_name, 0, duration_ms);

    // Introduce an artificial delay so peers have time to receive and start
    // playback.  On a typical LAN this is ~150-200 ms.  We schedule the
    // local playback slightly into the future using play_at.
    const HOST_SYNC_DELAY_MS: u64 = 0;
    let play_instant = std::time::Instant::now() + std::time::Duration::from_millis(HOST_SYNC_DELAY_MS);
    {
        let audio = state.audio_output.lock().await;
        if let Some(ref ao) = *audio {
            ao.play_at(play_instant);
        }
    }

    // Start the position ticker.
    start_position_ticker(app, state, duration_ms).await;

    // Evict decoded entries outside the new playback window, then
    // backfill any new window slots that are not yet decoded.
    evict_decoded_cache(state).await;
    backfill_decoded_cache(app, state).await;

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
                sample_rate: {
                    let audio = state.audio_output.lock().await;
                    audio.as_ref().map(|ao| ao.playback_state().sample_rate.load(std::sync::atomic::Ordering::Acquire)).unwrap_or(44100)
                },
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
            let host_sr = {
                let audio = state.audio_output.lock().await;
                audio.as_ref().map(|ao| ao.playback_state().sample_rate.load(std::sync::atomic::Ordering::Acquire)).unwrap_or(44100)
            };
            let target_time_ns = now_ns() + 50_000_000;
            broadcast_to_peers(&app, &Message::SeekCommand {
                position_samples: seek_samples,
                target_time_ns,
                sample_rate: host_sr,
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
            let file_data_for_decode = file_data.clone();
            let decoded = match tokio::task::spawn_blocking(move || {
                crate::audio::decoder::decode_mp3(&file_data_for_decode)
            }).await {
                Ok(Ok(d)) => d,
                Ok(Err(_e)) => return Err("Invalid or corrupted MP3 file.".into()),
                Err(e) => return Err(format!("Decode task failed: {e}")),
            };
            let duration_secs = decoded.duration_secs;

            // Add to queue first so we can check if it's in the cache window.
            let mut queue = state.queue.lock().await;
            let track_id = queue.add(file_name.clone(), duration_secs, "host".into());
            queue.mark_ready(track_id);
            let queue_items = queue.get_queue();
            drop(queue);

            // Store the raw file bytes so we can decode & play later.
            state.track_data.lock().await.insert(track_id, file_data.clone());

            // Only pre-resample and cache if the track is within the playback window.
            if is_in_cache_window(&state, track_id).await {
                let target_rate = *state.device_sample_rate.lock();
                let cached_decoded = if let Some(dev_rate) = target_rate {
                    if decoded.sample_rate != dev_rate {
                        log::info!("[add_song] Pre-resampling from {} Hz to {} Hz", decoded.sample_rate, dev_rate);
                        match tokio::task::spawn_blocking(move || {
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
                        }).await {
                            Ok(d) => d,
                            Err(e) => return Err(format!("Resample task failed: {e}")),
                        }
                    } else {
                        decoded
                    }
                } else {
                    decoded
                };

                state.decoded_cache.lock().await.insert(track_id, cached_decoded);
                log::info!("[add_song] Pre-cached decoded audio for {track_id}");
                evict_decoded_cache(&state).await;
            } else {
                log::info!("[add_song] Track {track_id} outside cache window, skipping pre-cache");
            }

            // Transfer the file to all connected peers in background.
            let peer_ids: Vec<u32> = host.peers.lock().keys().copied().collect();
            log::info!("[add_song] track_id={track_id} file={file_name} size={} peers={:?}", file_data.len(), peer_ids);
            let file_name_for_log = file_name.clone();
            if !peer_ids.is_empty() {
                let tcp_host = host.tcp_host.clone();
                drop(session);
                let app_for_transfer = app.clone();
                tokio::spawn(async move {
                    let state = app_for_transfer.state::<AppState>();
                    let mut mgr = state.file_transfer_mgr.lock().await;
                    if let Err(e) = mgr.start_transfer_with_id(
                        track_id,
                        file_name,
                        file_data,
                        &peer_ids,
                        &tcp_host,
                    ).await {
                        log::warn!("Failed to transfer file to peers: {e}");
                    }
                });
            } else {
                drop(session);
            }

            emit_queue_update(&app, &queue_items);

            log::info!("Added song \"{}\" to queue ({})", file_name_for_log, track_id);
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

            // Evict decoded entries outside the new playback window.
            evict_decoded_cache(&state).await;
            backfill_decoded_cache(&app, &state).await;

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
            // Evict decoded entries outside the new playback window.
            evict_decoded_cache(&state).await;
            backfill_decoded_cache(&app, &state).await;

            
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
                file_name: file_name.clone(),
                file_size: metadata.len(),
            };

            peer.send(&msg)
                .await
                .map_err(|e| format!("Failed to send song request: {e}"))?;

            // Store the file path so we can upload it when the host accepts.
            {
                let s = app.state::<AppState>();
                s.pending_request_paths.lock().insert(file_name, file_path.clone());
            }

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
            let msg = crate::network::messages::Message::SongRequestAccepted {
                request_id: id,
                file_name: req.file_name.clone(),
            };
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
/// Every ~2 seconds it also broadcasts a `PlaybackStateUpdate` to peers
/// so late-joining or behind peers can catch up.
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
        let mut tick_count: u64 = 0;

        loop {
            interval.tick().await;
            tick_count += 1;

            let (position_ms, position_frames, sr) = if let Some(ref ps) = playback_state {
                let pos_samples = ps.position.load(std::sync::atomic::Ordering::Acquire) as u64;
                let channels = ps.channels.load(std::sync::atomic::Ordering::Acquire) as u64;
                let sr = ps.sample_rate.load(std::sync::atomic::Ordering::Acquire) as u64;
                if sr > 0 && channels > 0 {
                    let frames = pos_samples / channels;
                    ((frames * 1000) / sr, frames, sr as u32)
                } else {
                    (0, 0, 0)
                }
            } else {
                (0, 0, 0)
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

            // Every 8 ticks (~2 seconds), broadcast full PlaybackStateUpdate
            // so peers that missed the initial PlayCommand can catch up.
            if tick_count % 8 == 0 {
                let s = app_clone.state::<AppState>();
                let ct = s.host_current_track.lock().clone();
                let tcp_host = s.host_tcp.lock().clone();
                if let (Some(tcp), Some(track)) = (tcp_host, ct) {
                    let msg = Message::PlaybackStateUpdate {
                        state: "playing".to_string(),
                        file_name: track.file_name.clone(),
                        position_ms,
                        duration_ms,
                        file_id: track.file_id,
                        position_samples: position_frames,
                        sample_rate: sr,
                    };
                    tcp.broadcast(&msg).await;
                }
            }
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

/// Peer-side position ticker: emits `playback:position` so the progress bar
/// updates smoothly. Does NOT auto-advance or broadcast to peers.
pub async fn start_peer_position_ticker(
    app: &AppHandle,
    state: &AppState,
) {
    // Stop any existing ticker first.
    stop_position_ticker(state).await;

    let app_clone = app.clone();
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
                break;
            };

            // Check if audio stopped (track ended or was stopped).
            let is_stopped = playback_state.as_ref().map_or(true, |ps| {
                ps.state.load(std::sync::atomic::Ordering::Acquire) == 0 // STATE_STOPPED
            });
            if is_stopped {
                break;
            }

            let _ = app_clone.emit(
                "playback:position",
                PlaybackPositionPayload {
                    position_ms,
                    duration_ms: 0, // Peer gets duration from state-changed events
                },
            );
        }
    });

    *state.position_ticker.lock().await = Some(handle);
}
