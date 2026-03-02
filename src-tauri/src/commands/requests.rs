use super::AppState;
use crate::network::messages::Message;
use crate::session::Session;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

// ── Song Request Commands ───────────────────────────────────────────────────

#[tauri::command]
pub async fn request_song(app: AppHandle, file_path: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut session = state.session.lock().await;

    match &mut *session {
        Session::Peer(peer) => {
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
            let msg = Message::SongRequestAccepted {
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
            let msg = Message::SongRequestRejected { request_id: id };
            host.send_to_peer(req.peer_id, &msg).await;

            log::info!("Rejected song request {} from peer {}", id, req.peer_id);
            Ok(())
        }
        _ => Err("Only the host can reject song requests.".into()),
    }
}
