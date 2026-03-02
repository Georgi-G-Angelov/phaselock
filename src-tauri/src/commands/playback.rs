use super::helpers::{
    backfill_decoded_cache, broadcast_to_peers, emit_error, emit_playback_state,
    emit_queue_update, evict_decoded_cache,
};
use super::{AppState, PlaybackPositionPayload};
use crate::audio::playback::AudioOutput;
use crate::network::messages::{CurrentTrack, Message};
use crate::network::udp::now_ns;
use crate::session::Session;
use tauri::{AppHandle, Emitter, Manager};

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

            // Update host_current_track BEFORE emitting, so the
            // PlaybackStateUpdate broadcast carries the correct position.
            {
                let mut ct = state.host_current_track.lock();
                if let Some(ref mut track) = *ct {
                    track.position_samples = current_pos;
                    track.is_playing = true;
                }
            }

            // Broadcast ResumeCommand to peers FIRST so it arrives
            // before the PlaybackStateUpdate that emit_playback_state sends.
            let target_time_ns = now_ns() + 50_000_000; // 50ms safety margin
            broadcast_to_peers(app, &Message::ResumeCommand {
                position_samples: current_pos,
                target_time_ns,
                sample_rate: sr,
            });

            emit_playback_state(app, "playing", &file_name, position_ms, duration_ms);

            // Restart the position ticker so the progress bar keeps updating.
            start_position_ticker(app, state, duration_ms).await;

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

            // Update shared current track state with paused position.
            {
                let mut ct = state.host_current_track.lock();
                if let Some(ref mut track) = *ct {
                    track.is_playing = false;
                    track.position_samples = pos_samples;
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

#[tauri::command]
pub async fn back(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let session = state.session.lock().await;

    match &*session {
        Session::Host(_host) => {
            drop(session);

            // Check how long the current song has been playing.
            let position_ms = {
                let audio = state.audio_output.lock().await;
                if let Some(ref ao) = *audio {
                    let pos = ao.get_position() as u64;
                    let sr = ao.playback_state().sample_rate.load(std::sync::atomic::Ordering::Acquire) as u64;
                    if sr > 0 { (pos * 1000) / sr } else { 0 }
                } else {
                    0
                }
            };

            if position_ms >= 5000 {
                // Restart current track — seek to 0.
                let audio = state.audio_output.lock().await;
                if let Some(ref ao) = *audio {
                    ao.seek(0);
                }
                drop(audio);

                let queue = state.queue.lock().await;
                let (file_name, duration_ms) = queue.current()
                    .map(|t| (t.file_name.clone(), (t.duration_secs * 1000.0) as u64))
                    .unwrap_or_default();
                drop(queue);

                emit_playback_state(&app, "playing", &file_name, 0, duration_ms);

                // Broadcast seek-to-start to peers.
                let sr = {
                    let audio = state.audio_output.lock().await;
                    audio.as_ref().map(|ao| ao.playback_state().sample_rate.load(std::sync::atomic::Ordering::Acquire)).unwrap_or(44100)
                };
                let target_time_ns = now_ns() + 50_000_000;
                broadcast_to_peers(&app, &Message::SeekCommand {
                    position_samples: 0,
                    target_time_ns,
                    sample_rate: sr,
                });

                log::info!("Back: restarted current track (was at {} ms)", position_ms);
            } else {
                // Go to previous track.
                stop_position_ticker(&state).await;
                let audio = state.audio_output.lock().await;
                if let Some(ref ao) = *audio {
                    ao.stop();
                }
                drop(audio);

                let mut queue = state.queue.lock().await;
                let has_prev = queue.previous().is_some();
                let queue_items = queue.get_queue();
                drop(queue);

                emit_queue_update(&app, &queue_items);

                if has_prev {
                    log::info!("Back: went to previous track");
                    play_current_track(&app, &state).await?;
                } else {
                    // Already at the start — just restart current track.
                    log::info!("Back: already at first track, restarting");
                    play_current_track(&app, &state).await?;
                }
            }
            Ok(())
        }
        _ => Err("Only the host can control playback.".into()),
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
pub(super) async fn start_position_ticker(
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
pub(super) async fn start_peer_position_ticker(
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
