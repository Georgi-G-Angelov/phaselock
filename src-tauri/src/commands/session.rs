use super::helpers::{
    backfill_decoded_cache, convert_sample_position, discovered_to_payload, emit_error,
    emit_queue_update, evict_decoded_cache, is_in_cache_window, latency_compensation_frames,
};
use super::playback::{start_peer_position_ticker, stop_position_ticker};
use super::{
    AppState, DiscoveredSessionPayload, ListenerInfo, ListenersUpdatedPayload,
    PeerJoinedPayload, PeerLeftPayload, PlaybackStatePayload, QueueUpdatedPayload,
    SessionInfoPayload, SyncLatencyPayload, SyncStatePayload,
};
use crate::audio::playback::AudioOutput;
use crate::network::messages::Message;
use crate::session::Session;
use crate::transfer::file_transfer::FileReceiver;
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

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
                            // Clear per-peer transfer tracking so retransfer works on reconnect.
                            let s = app_clone.state::<AppState>();
                            {
                                let mut mgr = s.file_transfer_mgr.lock().await;
                                mgr.cancel_peer(peer_id);
                            }
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
                                    // Only transfer the next TRANSFER_WINDOW files the peer is missing
                                    // to avoid saturating the network with bulk transfers.
                                    use super::helpers::TRANSFER_WINDOW;
                                    log::info!("[host] Received FileCacheReport from peer {peer_id} with {} cached file(s)", file_ids.len());
                                    let s = app_clone.state::<AppState>();
                                    let queue = s.queue.lock().await;
                                    let upcoming_ids: Vec<Uuid> = queue.upcoming_ids(TRANSFER_WINDOW)
                                        .into_iter().collect();
                                    log::info!("[host] Queue has {} track(s), upcoming window: {:?}", queue.get_queue().len(), upcoming_ids);
                                    drop(queue);

                                    let cached_set: std::collections::HashSet<Uuid> = file_ids.into_iter().collect();
                                    let missing: Vec<Uuid> = upcoming_ids.into_iter()
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

                                                // Parse ID3 metadata for title/artist.
                                                let file_data_for_meta = file_data.clone();
                                                let metadata = tokio::task::spawn_blocking(move || {
                                                    crate::audio::decoder::parse_mp3_metadata(&file_data_for_meta)
                                                }).await.unwrap_or_default();
                                                let (title, artist) = crate::audio::decoder::resolve_track_info(&metadata, &file_name);
                                                log::info!("[host] Upload metadata: title=\"{title}\" artist=\"{artist}\"");

                                                // Add to queue first (before cache decision) so
                                                // we can check whether this track is in the window.
                                                let track_id = request_id;
                                                let mut queue = s.queue.lock().await;
                                                queue.add_with_id(track_id, file_name.clone(), title, artist, duration_secs, format!("peer {peer_id}"));
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

                                                // Transfer the file to other peers (exclude the uploader),
                                                // but only if the track falls within the transfer window.
                                                let in_transfer_window = {
                                                    let q = s.queue.lock().await;
                                                    q.upcoming_ids(super::helpers::TRANSFER_WINDOW).contains(&track_id)
                                                };

                                                if in_transfer_window {
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
                                                } else {
                                                    log::info!("[host] Uploaded track {track_id} outside transfer window, skipping peer relay");
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
                // Periodic timer for proactive file-sync: every 10 seconds the
                // peer checks whether it has the next 5 queue items cached and,
                // if not, sends a FileCacheReport so the host retransfers the
                // missing files.
                let mut file_sync_interval = tokio::time::interval(std::time::Duration::from_secs(10));
                // Consume the first (immediate) tick so the first real check
                // happens 10 s after joining.
                file_sync_interval.tick().await;

                loop {
                    tokio::select! {
                        event = event_rx.recv() => {
                            let Some(event) = event else { break };
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
                                                    log::warn!("[peer] Catch-up: file {file_id} not in cache yet — requesting from host");
                                                    // Immediately send a FileCacheReport so the host
                                                    // transfers this file instead of waiting for the
                                                    // periodic 10-second file-sync timer.
                                                    // Clone the TCP writer Arc and drop the session
                                                    // lock before the async send to avoid holding
                                                    // session across an await point.
                                                    let cache = s.file_cache.lock().await;
                                                    let cached_ids = cache.cached_ids();
                                                    drop(cache);
                                                    let writer = {
                                                        let session = s.session.lock().await;
                                                        if let Session::Peer(ref peer) = *session {
                                                            Some(peer.tcp_peer.writer.clone())
                                                        } else {
                                                            None
                                                        }
                                                    };
                                                    if let Some(writer) = writer {
                                                        let msg = Message::FileCacheReport { file_ids: cached_ids };
                                                        let mut guard = writer.lock().await;
                                                        if let Some(ref mut w) = *guard {
                                                            let _ = crate::network::messages::write_message(w, &msg).await;
                                                        }
                                                    }
                                                }
                                            }
                                        } else if same_file && (peer_state == PlaybackStateEnum::Playing || peer_state == PlaybackStateEnum::Waiting) {
                                            // ── Drift correction: peer is playing the same file, check if out of sync ──
                                            // Skip drift correction while files are still transferring — the
                                            // network congestion makes latency measurements unreliable.
                                            let s = app_clone.state::<AppState>();
                                            {
                                                let receiver = s.file_receiver.lock().await;
                                                if receiver.active_incoming_count() > 0 {
                                                    log::debug!("[peer] Skipping drift correction — {} file(s) still transferring", receiver.active_incoming_count());
                                                    // Still emit the UI event below.
                                                    let _ = app_clone.emit(
                                                        "playback:state-changed",
                                                        PlaybackStatePayload {
                                                            state,
                                                            file_name,
                                                            position_ms,
                                                            duration_ms,
                                                        },
                                                    );
                                                    continue;
                                                }
                                            }

                                            let latency_ns = {
                                                let session = s.session.lock().await;
                                                if let Session::Peer(ref peer) = *session {
                                                    peer.clock_sync.lock().current_latency_ns
                                                } else { 0 }
                                            };

                                            let audio = s.audio_output.lock().await;
                                            if let Some(ref ao) = *audio {
                                                let peer_sr = ao.device_sample_rate;
                                                let peer_pos_frames = ao.get_position();

                                                // Compute expected position: host position converted
                                                // to peer sample rate, plus latency & processing compensation.
                                                let processing_ns = received_at.elapsed().as_nanos() as u64;
                                                let total_delay_ns = latency_ns + processing_ns;
                                                let expected_frames = convert_sample_position(position_samples, host_sr, peer_sr)
                                                    + latency_compensation_frames(total_delay_ns, peer_sr);

                                                // Compute drift in milliseconds.
                                                let drift_frames = expected_frames as i64 - peer_pos_frames as i64;
                                                let drift_ms = if peer_sr > 0 {
                                                    drift_frames as f64 / peer_sr as f64 * 1000.0
                                                } else {
                                                    0.0
                                                };

                                                const DRIFT_THRESHOLD_MS: f64 = 50.0;

                                                if drift_ms.abs() > DRIFT_THRESHOLD_MS {
                                                    log::info!(
                                                        "[peer] Drift correction: {:.1} ms off (expected frame {expected_frames}, actual {peer_pos_frames}) — seeking to correct (latency {:.2} ms + processing {:.2} ms)",
                                                        drift_ms,
                                                        latency_ns as f64 / 1_000_000.0,
                                                        processing_ns as f64 / 1_000_000.0,
                                                    );
                                                    ao.seek(expected_frames);
                                                } else {
                                                    log::debug!(
                                                        "[peer] Drift check: {:.1} ms — within threshold, no correction needed",
                                                        drift_ms
                                                    );
                                                }
                                            }
                                        }
                                        // else: other states — no action needed.
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
                    } // end event arm

                    // ── Periodic file-sync arm ──────────────────────────
                    _ = file_sync_interval.tick() => {
                        use super::helpers::TRANSFER_WINDOW;
                        let s = app_clone.state::<AppState>();
                        let queue = s.queue.lock().await;
                        // Use the same window the host uses, based on queue position.
                        let upcoming: Vec<uuid::Uuid> = queue.upcoming_ids(TRANSFER_WINDOW)
                            .into_iter().collect();
                        drop(queue);

                        if upcoming.is_empty() {
                            continue;
                        }

                        let cache = s.file_cache.lock().await;
                        let missing: Vec<uuid::Uuid> = upcoming.iter()
                            .filter(|id| !cache.has(id))
                            .copied()
                            .collect();
                        let cached_ids = cache.cached_ids();
                        drop(cache);

                        if !missing.is_empty() {
                            log::info!(
                                "[peer] File-sync check: missing {} of {} upcoming file(s): {:?}",
                                missing.len(),
                                upcoming.len(),
                                missing,
                            );
                            // Send a FileCacheReport so the host transfers the missing files.
                            // Clone the TCP writer Arc and drop the session lock before
                            // the async send to avoid holding session across an await.
                            let writer = {
                                let session = s.session.lock().await;
                                if let Session::Peer(ref peer) = *session {
                                    Some(peer.tcp_peer.writer.clone())
                                } else {
                                    None
                                }
                            };
                            if let Some(writer) = writer {
                                let msg = Message::FileCacheReport { file_ids: cached_ids };
                                let mut guard = writer.lock().await;
                                if let Some(ref mut w) = *guard {
                                    let _ = crate::network::messages::write_message(w, &msg).await;
                                }
                            }
                        }
                    }
                    } // end select!
                } // end loop
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

    // Take the session out of the mutex so we can drop the lock before
    // acquiring queue / track_data / etc.  This prevents lock-inversion
    // deadlocks with backfill_decoded_cache (which takes queue → session).
    let mut old_session = {
        let mut session = state.session.lock().await;
        if !session.is_active() {
            return Err("No active session to leave.".into());
        }
        std::mem::replace(&mut *session, Session::None)
    };

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

    old_session.shutdown().await;

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
        use crate::network::mdns::MdnsBrowser;
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
