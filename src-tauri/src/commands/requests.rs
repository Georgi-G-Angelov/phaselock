use super::AppState;
use crate::network::messages::Message;
use crate::session::Session;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

// ── Song Request Commands ───────────────────────────────────────────────────

/// Resolve metadata for a song before requesting it.
/// Returns `{ display_name, content }` — the content may be transformed
/// (e.g. Spotify → "artist - title" search query).
#[tauri::command]
pub async fn resolve_song_meta(kind: String, content: String) -> Result<ResolvedMeta, String> {
    match kind.as_str() {
        "youtube_url" => {
            let meta = crate::youtube::download::fetch_metadata(content.trim()).await?;
            let display = format!("{} – {}", meta.artist, meta.title);
            Ok(ResolvedMeta { display_name: display, content: content.trim().to_string() })
        }
        "spotify_track" => {
            let url = content.trim().to_string();
            let track = crate::youtube::spotify::fetch_track_info(&url).await?;
            let display = format!("{} – {}", track.artist, track.name);
            // The host will search YouTube for this query when accepted.
            Ok(ResolvedMeta { display_name: display, content: url })
        }
        "youtube_search" => {
            // The query IS the display name.
            let q = content.trim().to_string();
            Ok(ResolvedMeta { display_name: q.clone(), content: q })
        }
        "file" => {
            let name = std::path::Path::new(content.trim())
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            Ok(ResolvedMeta { display_name: name, content: content.trim().to_string() })
        }
        _ => Err(format!("Unknown kind: {kind}")),
    }
}

#[derive(Clone, serde::Serialize)]
pub struct ResolvedMeta {
    pub display_name: String,
    pub content: String,
}

/// Peer command: request a song by file path, YouTube URL, search query, or
/// Spotify track link.
///
/// `kind` must be one of: `"file"`, `"youtube_url"`, `"youtube_search"`,
/// `"spotify_track"`.
/// `display_name` is the resolved human-readable song name.
#[tauri::command]
pub async fn request_song(
    app: AppHandle,
    kind: String,
    content: String,
    display_name: String,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut session = state.session.lock().await;

    match &mut *session {
        Session::Peer(peer) => {
            match kind.as_str() {
                "file" => {
                    use std::path::Path;
                    let file_path = content.clone();
                    let path = Path::new(&file_path);
                    let file_name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();

                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if !ext.eq_ignore_ascii_case("mp3") {
                        return Err("Only .mp3 files are supported.".into());
                    }

                    let metadata = tokio::fs::metadata(&file_path)
                        .await
                        .map_err(|e| format!("Failed to read file metadata: {e}"))?;

                    const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;
                    if metadata.len() > MAX_FILE_SIZE {
                        return Err("File is too large. Maximum size is 50 MB.".into());
                    }

                    let msg = Message::SongRequest {
                        file_name: file_name.clone(),
                        file_size: metadata.len(),
                        kind: "file".into(),
                        display_name: display_name.clone(),
                    };
                    peer.send(&msg).await
                        .map_err(|e| format!("Failed to send song request: {e}"))?;

                    state.pending_request_paths.lock().insert(file_name, file_path.clone());
                    log::info!("Sent file song request for \"{file_path}\"");
                }
                "youtube_url" => {
                    let url = content.trim().to_string();
                    if url.is_empty() {
                        return Err("URL cannot be empty.".into());
                    }
                    let msg = Message::SongRequest {
                        file_name: url.clone(),
                        file_size: 0,
                        kind: "youtube_url".into(),
                        display_name: display_name.clone(),
                    };
                    peer.send(&msg).await
                        .map_err(|e| format!("Failed to send song request: {e}"))?;
                    log::info!("Sent YouTube URL request: \"{url}\"");
                }
                "youtube_search" => {
                    let query = content.trim().to_string();
                    if query.is_empty() {
                        return Err("Search query cannot be empty.".into());
                    }
                    let msg = Message::SongRequest {
                        file_name: query.clone(),
                        file_size: 0,
                        kind: "youtube_search".into(),
                        display_name: display_name.clone(),
                    };
                    peer.send(&msg).await
                        .map_err(|e| format!("Failed to send song request: {e}"))?;
                    log::info!("Sent YouTube search request: \"{query}\"");
                }
                "spotify_track" => {
                    let url = content.trim().to_string();
                    if !crate::youtube::spotify::is_spotify_track_url(&url) {
                        return Err("Not a valid Spotify track URL.".into());
                    }
                    if crate::youtube::spotify::is_spotify_playlist_url(&url) {
                        return Err("Playlist requests are not allowed. Please request individual tracks.".into());
                    }
                    let msg = Message::SongRequest {
                        file_name: url.clone(),
                        file_size: 0,
                        kind: "spotify_track".into(),
                        display_name: display_name.clone(),
                    };
                    peer.send(&msg).await
                        .map_err(|e| format!("Failed to send song request: {e}"))?;
                    log::info!("Sent Spotify track request: \"{url}\"");
                }
                _ => return Err(format!("Unknown request kind: {kind}")),
            }
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
            let kind = req.kind.clone();
            let content = req.file_name.clone();

            // Send acceptance to the requesting peer.
            let msg = Message::SongRequestAccepted {
                request_id: id,
                file_name: req.file_name.clone(),
                kind: kind.clone(),
            };
            host.send_to_peer(req.peer_id, &msg).await;
            drop(requests);
            drop(session);

            log::info!("Accepted song request {} (kind={kind}) from peer {}", id, req.peer_id);

            // For non-file requests, trigger host-side processing immediately.
            match kind.as_str() {
                "file" => { /* Peer will upload file chunks — nothing to do here. */ }
                "youtube_url" => {
                    state.download_queue.enqueue(content.clone()).await;
                    let snapshot = state.download_queue.snapshot().await;
                    super::queue::emit_download_queue(&app, &snapshot);
                    log::info!("[accept] Enqueued YouTube URL: {content}");
                }
                "youtube_search" => {
                    let dl_id = state.download_queue.enqueue_search(content.clone()).await;
                    state.search_queue.enqueue(content.clone(), dl_id).await;
                    let snapshot = state.download_queue.snapshot().await;
                    super::queue::emit_download_queue(&app, &snapshot);
                    log::info!("[accept] Enqueued YouTube search: \"{content}\"");
                }
                "spotify_track" => {
                    use crate::youtube::spotify;
                    match spotify::fetch_track_info(&content).await {
                        Ok(track) => {
                            let query = format!("{} - {}", track.artist, track.name);
                            let dl_id = state.download_queue.enqueue_search(query.clone()).await;
                            state.search_queue.enqueue(query, dl_id).await;
                            let snapshot = state.download_queue.snapshot().await;
                            super::queue::emit_download_queue(&app, &snapshot);
                            log::info!("[accept] Spotify track → YouTube search enqueued");
                        }
                        Err(e) => {
                            log::error!("[accept] Failed to fetch Spotify track info: {e}");
                        }
                    }
                }
                other => {
                    log::warn!("[accept] Unknown request kind: {other}");
                }
            }

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
            let msg = Message::SongRequestRejected { request_id: id };
            host.send_to_peer(req.peer_id, &msg).await;

            log::info!("Rejected song request {} from peer {}", id, req.peer_id);
            Ok(())
        }
        _ => Err("Only the host can reject song requests.".into()),
    }
}
