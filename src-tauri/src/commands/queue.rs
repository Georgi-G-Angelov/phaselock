use super::helpers::{
    backfill_decoded_cache, emit_queue_update, evict_decoded_cache, is_in_cache_window,
};
use super::AppState;
use crate::audio::playback::AudioOutput;
use crate::session::Session;
use tauri::{AppHandle, Manager};
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
