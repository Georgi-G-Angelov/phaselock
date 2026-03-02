use super::{
    AppState, DiscoveredSessionPayload, ErrorPayload, PlaybackStatePayload,
    QueueUpdatedPayload,
};
use crate::audio::decoder::DecodedAudio;
use crate::audio::playback::AudioOutput;
use crate::network::mdns::DiscoveredSession;
use crate::network::messages::{Message, QueueItem};
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

// ── Emit helpers ────────────────────────────────────────────────────────────

pub(super) fn emit_error(app: &AppHandle, msg: &str) {
    let _ = app.emit("error:general", ErrorPayload { message: msg.to_string() });
}

/// Convert a sample position from one sample rate to another.
/// E.g. frame 44100 at 44100 Hz = 1 second = frame 48000 at 48000 Hz.
pub(super) fn convert_sample_position(position_frames: u64, src_rate: u32, dst_rate: u32) -> u64 {
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
pub(super) fn latency_compensation_frames(latency_ns: u64, sample_rate: u32) -> u64 {
    // frames = latency_seconds * sample_rate
    (latency_ns as f64 / 1_000_000_000.0 * sample_rate as f64).round() as u64
}

pub(super) fn emit_queue_update(app: &AppHandle, queue: &[QueueItem]) {
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

pub(super) fn emit_playback_state(app: &AppHandle, pstate: &str, file_name: &str, position_ms: u64, duration_ms: u64) {
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
    // Read file_name AND file_id from the same atomic snapshot of
    // `host_current_track` to avoid mismatches during rapid skipping.
    let state = app.state::<AppState>();
    let tcp_host = state.host_tcp.lock().clone();
    if let Some(tcp_host) = tcp_host {
        let ct = state.host_current_track.lock().clone();
        if let Some(track) = ct {
            let msg = Message::PlaybackStateUpdate {
                state: pstate.to_string(),
                file_name: track.file_name.clone(),
                position_ms,
                duration_ms,
                file_id: track.file_id,
                position_samples: track.position_samples,
                sample_rate: track.sample_rate,
            };
            tokio::spawn(async move {
                tcp_host.broadcast(&msg).await;
            });
        }
    }
}

/// Broadcast any message to all connected peers (no-op if not host).
pub(super) fn broadcast_to_peers(app: &AppHandle, msg: &Message) {
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

// ── Decoded-audio cache helpers ─────────────────────────────────────────────

/// Maximum number of decoded tracks to keep in memory (playing + next N).
pub(super) const DECODED_CACHE_WINDOW: usize = 5;

/// Maximum number of tracks (from the current position) the host will
/// proactively transfer to peers.  Keeps the network from being saturated
/// when a peer joins a large queue.  The peer's periodic file-sync timer
/// will request more files as it advances through the queue.
pub(super) const TRANSFER_WINDOW: usize = 5;

/// Check whether a track id falls within the current decoded-cache window.
///
/// Returns `true` when the track is among the currently playing track and
/// the next `DECODED_CACHE_WINDOW - 1` tracks in the queue, meaning it is
/// worth pre-decoding.
pub(super) async fn is_in_cache_window(state: &AppState, track_id: Uuid) -> bool {
    let queue = state.queue.lock().await;
    queue.upcoming_ids(DECODED_CACHE_WINDOW).contains(&track_id)
}

/// Evict decoded audio entries that fall outside the playback window.
///
/// Keeps only the currently playing track and the next `DECODED_CACHE_WINDOW - 1`
/// tracks in the queue. Must be called after any decoded_cache insertion or
/// queue mutation (advance, reorder, remove).
pub(super) async fn evict_decoded_cache(state: &AppState) {
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
pub(super) async fn backfill_decoded_cache(app: &AppHandle, state: &AppState) {
    // Determine which IDs are in the window but not yet in the cache.
    // NOTE: we deliberately do NOT hold `queue` and `session` simultaneously
    // to avoid lock-inversion deadlocks (add_song and leave_session take
    // them in the opposite order).
    let window_ids = {
        let queue = state.queue.lock().await;
        queue.upcoming_ids(DECODED_CACHE_WINDOW)
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
        // Get the raw bytes — try host's track_data first, then peer's
        // file_cache.  This avoids needing a session lock to check is_host.
        let raw_bytes: Option<Vec<u8>> = {
            let store = state.track_data.lock().await;
            store.get(&file_id).cloned()
        };
        let raw_bytes = if raw_bytes.is_none() {
            let cache = state.file_cache.lock().await;
            cache.get(&file_id).cloned()
        } else {
            raw_bytes
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
                        return Ok(DecodedAudio {
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

// ── Discovery helpers ───────────────────────────────────────────────────────

pub(super) fn discovered_to_payload(d: &DiscoveredSession) -> DiscoveredSessionPayload {
    DiscoveredSessionPayload {
        session_name: d.session_name.clone(),
        host_name: d.host_name.clone(),
        address: d.address.to_string(),
        peer_count: d.peer_count,
        max_peers: d.max_peers,
    }
}
