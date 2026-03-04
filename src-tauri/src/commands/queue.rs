use super::helpers::{
    backfill_decoded_cache, emit_queue_update, evict_decoded_cache, is_in_cache_window,
};
use super::AppState;
use crate::audio::playback::AudioOutput;
use crate::session::Session;
use crate::youtube::queue::DownloadQueueSnapshot;
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

// ── Queue Commands ──────────────────────────────────────────────────────────

#[tauri::command]
pub async fn add_song(app: AppHandle, file_path: String) -> Result<(), String> {
    let state = app.state::<AppState>();

    // Extract what we need from the session (peer list + TCP handle) and
    // drop the lock immediately.  Holding `session` across file I/O and
    // decode would create a lock-inversion deadlock with
    // `backfill_decoded_cache` which takes `queue → session`.
    let (peer_ids, tcp_host_arc) = {
        let session = state.session.lock().await;
        match &*session {
            Session::Host(host) => {
                let ids: Vec<u32> = host.peers.lock().keys().copied().collect();
                let tcp = host.tcp_host.clone();
                (ids, tcp)
            }
            _ => return Err("Only the host can add songs directly.".into()),
        }
    };

    {
        {
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
                crate::audio::decoder::decode_audio(&file_data_for_decode)
            }).await {
                Ok(Ok(d)) => d,
                Ok(Err(_e)) => return Err("Invalid or corrupted MP3 file.".into()),
                Err(e) => return Err(format!("Decode task failed: {e}")),
            };
            let duration_secs = decoded.duration_secs;

            // Parse ID3 metadata for title/artist.
            let file_data_for_meta = file_data.clone();
            let metadata = tokio::task::spawn_blocking(move || {
                crate::audio::decoder::parse_mp3_metadata(&file_data_for_meta)
            }).await.unwrap_or_default();
            let (title, artist) = crate::audio::decoder::resolve_track_info(&metadata, &file_name);
            log::info!("[add_song] Metadata: title=\"{title}\" artist=\"{artist}\"");

            // Add to queue first so we can check if it's in the cache window.
            let mut queue = state.queue.lock().await;
            let track_id = queue.add(file_name.clone(), title, artist, duration_secs, "host".into());
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

            // Transfer the file to all connected peers, but only if the
            // newly added track falls within the upcoming transfer window.
            // Peers will request files past the window via their periodic
            // file-sync timer as the queue advances.
            use super::helpers::TRANSFER_WINDOW;
            let in_window = is_in_cache_window(&state, track_id).await
                || {
                    let q = state.queue.lock().await;
                    q.upcoming_ids(TRANSFER_WINDOW).contains(&track_id)
                };

            log::info!("[add_song] track_id={track_id} file={file_name} size={} peers={:?} in_window={in_window}", file_data.len(), peer_ids);
            let file_name_for_log = file_name.clone();
            if !peer_ids.is_empty() && in_window {
                let app_for_transfer = app.clone();
                let tcp_host = tcp_host_arc;
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
                if !in_window {
                    log::info!("[add_song] Track {track_id} outside transfer window, skipping peer transfer");
                }
            }

            emit_queue_update(&app, &queue_items);

            log::info!("Added song \"{}\" to queue ({})", file_name_for_log, track_id);
            Ok(())
        }
    }
}

#[tauri::command]
pub async fn remove_from_queue(app: AppHandle, track_id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    if !matches!(&*session, Session::Host(_)) {
        return Err("Only the host can modify the queue.".into());
    }
    drop(session);

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

#[tauri::command]
pub async fn reorder_queue(app: AppHandle, from_index: usize, to_index: usize) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    if !matches!(&*session, Session::Host(_)) {
        return Err("Only the host can reorder the queue.".into());
    }
    drop(session);

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

#[tauri::command]
pub async fn shuffle_queue(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    if !matches!(&*session, Session::Host(_)) {
        return Err("Only the host can shuffle the queue.".into());
    }
    drop(session);

    let mut queue = state.queue.lock().await;
    queue.shuffle_upcoming();
    let queue_items = queue.get_queue();
    drop(queue);

    evict_decoded_cache(&state).await;
    backfill_decoded_cache(&app, &state).await;

    emit_queue_update(&app, &queue_items);

    log::info!("Shuffled upcoming queue");
    Ok(())
}

// ── YouTube ─────────────────────────────────────────────────────────────────

/// Payload emitted to the frontend when the download queue changes.
#[derive(Clone, serde::Serialize)]
pub(super) struct DownloadQueuePayload {
    pub tasks: Vec<crate::youtube::queue::DownloadTask>,
}

/// Emit the current download queue to the frontend.
pub(super) fn emit_download_queue(app: &AppHandle, snapshot: &DownloadQueueSnapshot) {
    let _ = app.emit("youtube:download-queue", DownloadQueuePayload {
        tasks: snapshot.tasks.clone(),
    });
}

/// Tauri command: instantly enqueue a YouTube URL for background processing.
#[tauri::command]
pub async fn enqueue_youtube(app: AppHandle, url: String) -> Result<(), String> {
    let state = app.state::<AppState>();

    // Verify host role.
    {
        let session = state.session.lock().await;
        if !matches!(&*session, Session::Host(_)) {
            return Err("Only the host can add songs.".into());
        }
    }

    // Add to download queue immediately with unknown metadata.
    state.download_queue.enqueue(url.clone()).await;

    // Emit updated queue to frontend.
    let snapshot = state.download_queue.snapshot().await;
    emit_download_queue(&app, &snapshot);

    log::info!("[enqueue_youtube] Queued URL: {url}");
    Ok(())
}

/// Tauri command: instantly enqueue a YouTube search query for background processing.
#[tauri::command]
pub async fn search_youtube(app: AppHandle, query: String) -> Result<(), String> {
    let state = app.state::<AppState>();

    // Verify host role.
    {
        let session = state.session.lock().await;
        if !matches!(&*session, Session::Host(_)) {
            return Err("Only the host can add songs.".into());
        }
    }

    let query = query.trim().to_string();
    if query.is_empty() {
        return Err("Search query cannot be empty.".into());
    }

    // Create a download task immediately so it appears in the UI.
    let dl_id = state.download_queue.enqueue_search(query.clone()).await;

    // Emit updated queue to frontend so user sees the "Searching" task instantly.
    let snapshot = state.download_queue.snapshot().await;
    emit_download_queue(&app, &snapshot);

    // Enqueue the search query (linked to the download task we just created).
    state.search_queue.enqueue(query.clone(), dl_id).await;

    log::info!("[search_youtube] Queued search: \"{query}\" (download task #{dl_id})");
    Ok(())
}

/// Tauri command: import from Spotify — accepts a playlist or track URL.
/// For playlists, fetch all tracks and enqueue each as a YouTube search.
/// For single tracks, enqueue one YouTube search.
/// Returns the number of tracks enqueued.
#[tauri::command]
pub async fn import_spotify(app: AppHandle, url: String) -> Result<u32, String> {
    let state = app.state::<AppState>();

    // Verify host role.
    {
        let session = state.session.lock().await;
        if !matches!(&*session, Session::Host(_)) {
            return Err("Only the host can add songs.".into());
        }
    }

    let url = url.trim().to_string();
    use crate::youtube::spotify;

    let tracks: Vec<spotify::SpotifyTrack> = if spotify::is_spotify_playlist_url(&url) {
        log::info!("[import_spotify] Fetching playlist: {url}");
        spotify::fetch_playlist_tracks(&url).await?
    } else if spotify::is_spotify_track_url(&url) {
        log::info!("[import_spotify] Fetching single track: {url}");
        let track = spotify::fetch_track_info(&url).await?;
        vec![track]
    } else {
        return Err("Not a valid Spotify playlist or track URL.".into());
    };

    let count = tracks.len() as u32;
    log::info!("[import_spotify] Found {count} track(s), enqueuing searches");

    for track in tracks {
        let query = format!("{} - {}", track.artist, track.name);
        let dl_id = state.download_queue.enqueue_search(query.clone()).await;
        state.search_queue.enqueue(query, dl_id).await;
    }

    let snapshot = state.download_queue.snapshot().await;
    emit_download_queue(&app, &snapshot);

    log::info!("[import_spotify] Enqueued {count} YouTube search(es)");
    Ok(count)
}

/// Process a single download task: download audio, decode, add to queue, transfer to peers.
///
/// Called by the background worker — NOT a Tauri command.
pub(super) async fn process_youtube_download(
    app: &AppHandle,
    task_url: &str,
    _task_title: &str,
    _task_artist: &str,
) -> Result<(), String> {
    let state = app.state::<AppState>();

    // Extract peer info (may have changed since enqueue).
    let (peer_ids, tcp_host_arc) = {
        let session = state.session.lock().await;
        match &*session {
            Session::Host(host) => {
                let ids: Vec<u32> = host.peers.lock().keys().copied().collect();
                let tcp = host.tcp_host.clone();
                (ids, tcp)
            }
            _ => return Err("Session is no longer active.".into()),
        }
    };

    // Download audio.
    let track = crate::youtube::download::download_audio(task_url).await?;
    let title = track.title.clone();
    let artist = track.artist.clone();
    let file_name = track.file_name.clone();
    let audio_data = track.audio_data;

    const MAX_FILE_SIZE: usize = 50 * 1024 * 1024;
    if audio_data.len() > MAX_FILE_SIZE {
        return Err("Downloaded audio is too large (>50 MB).".into());
    }

    // Decode to PCM.
    let audio_data_for_decode = audio_data.clone();
    let decoded = match tokio::task::spawn_blocking(move || {
        crate::audio::decoder::decode_audio(&audio_data_for_decode)
    }).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return Err(format!("Failed to decode YouTube audio: {e}")),
        Err(e) => return Err(format!("Decode task failed: {e}")),
    };
    let duration_secs = decoded.duration_secs;

    log::info!(
        "[yt_worker] title=\"{title}\" artist=\"{artist}\" duration={duration_secs:.1}s size={}",
        audio_data.len()
    );

    // Add to playback queue.
    let mut queue = state.queue.lock().await;
    let track_id = queue.add(file_name.clone(), title, artist, duration_secs, "host".into());
    queue.mark_ready(track_id);
    let queue_items = queue.get_queue();
    drop(queue);

    // Store raw audio bytes for peer transfer.
    state.track_data.lock().await.insert(track_id, audio_data.clone());

    // Pre-cache decoded PCM if within playback window.
    if is_in_cache_window(&state, track_id).await {
        let target_rate = *state.device_sample_rate.lock();
        let cached_decoded = if let Some(dev_rate) = target_rate {
            if decoded.sample_rate != dev_rate {
                log::info!("[yt_worker] Resampling {} → {} Hz", decoded.sample_rate, dev_rate);
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
        log::info!("[yt_worker] Pre-cached decoded audio for {track_id}");
        evict_decoded_cache(&state).await;
    }

    // Transfer to connected peers (if within transfer window).
    use super::helpers::TRANSFER_WINDOW;
    let in_window = is_in_cache_window(&state, track_id).await
        || {
            let q = state.queue.lock().await;
            q.upcoming_ids(TRANSFER_WINDOW).contains(&track_id)
        };

    if !peer_ids.is_empty() && in_window {
        let app_for_transfer = app.clone();
        let tcp_host = tcp_host_arc;
        tokio::spawn(async move {
            let state = app_for_transfer.state::<AppState>();
            let mut mgr = state.file_transfer_mgr.lock().await;
            if let Err(e) = mgr.start_transfer_with_id(
                track_id,
                file_name,
                audio_data,
                &peer_ids,
                &tcp_host,
            ).await {
                log::warn!("Failed to transfer YouTube audio to peers: {e}");
            }
        });
    }

    emit_queue_update(&app, &queue_items);

    log::info!("[yt_worker] Added YouTube track {track_id} to queue");
    Ok(())
}

// Keep the old command name as an alias so registered invoke_handler still works.
#[tauri::command]
pub async fn add_youtube_song(app: AppHandle, url: String) -> Result<(), String> {
    enqueue_youtube(app, url).await
}
