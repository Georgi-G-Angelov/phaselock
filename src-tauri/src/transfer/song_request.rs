use crate::transfer::file_transfer::compute_sha256;
use std::collections::HashMap;
use uuid::Uuid;

// ── Constants ───────────────────────────────────────────────────────────────

/// Maximum file size: 50 MB.
const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;

/// Chunk size used on the peer side when uploading: 64 KB.
pub const UPLOAD_CHUNK_SIZE: usize = 64 * 1024;

// ── Data structures ─────────────────────────────────────────────────────────

/// A pending song request awaiting host accept/reject.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub request_id: Uuid,
    pub peer_id: u32,
    pub peer_name: String,
    pub file_name: String,
    pub file_size: u64,
}

/// An in-progress upload from a peer to the host.
#[derive(Debug)]
struct IncomingUpload {
    request_id: Uuid,
    file_name: String,
    peer_id: u32,
    expected_size: u64,
    data: Vec<u8>,
    bytes_received: u64,
}

/// Result of completing an upload (hash verified).
#[derive(Debug)]
pub enum UploadResult {
    /// File received, hash verified, ready for queueing.
    Success {
        request_id: Uuid,
        file_name: String,
        peer_id: u32,
        file_data: Vec<u8>,
    },
    /// Hash mismatch – file is corrupt.
    HashMismatch {
        request_id: Uuid,
    },
}

/// Reason a request was auto-rejected.
#[derive(Debug, PartialEq)]
pub enum AutoRejectReason {
    FileTooLarge(u64),
}

// ── SongRequestManager ─────────────────────────────────────────────────────

/// Manages song request flow on the **host** side.
///
/// Lifecycle:
///   1. `receive_request()` — stores a `PendingRequest`.
///   2. `accept()` / `reject()` — moves or removes the request.
///   3. On accept the peer uploads chunks via `receive_chunk()`.
///   4. `complete_upload()` — verifies the SHA-256 hash and returns data.
pub struct SongRequestManager {
    pending_requests: Vec<PendingRequest>,
    active_uploads: HashMap<Uuid, IncomingUpload>,
}

impl SongRequestManager {
    pub fn new() -> Self {
        Self {
            pending_requests: Vec::new(),
            active_uploads: HashMap::new(),
        }
    }

    /// Reset to initial empty state.
    pub fn clear(&mut self) {
        self.pending_requests.clear();
        self.active_uploads.clear();
    }

    // ── Request intake ──────────────────────────────────────────────────

    /// Called when a `SongRequest` message arrives from a peer.
    ///
    /// If the file is too large it is auto-rejected and returns `Err` with
    /// the reason.  Otherwise it generates a `request_id`, stores the
    /// request, and returns `Ok(PendingRequest)`.
    pub fn receive_request(
        &mut self,
        peer_id: u32,
        peer_name: String,
        file_name: String,
        file_size: u64,
    ) -> Result<PendingRequest, AutoRejectReason> {
        if file_size > MAX_FILE_SIZE {
            log::warn!(
                "Auto-rejecting song request from peer {peer_id}: file too large ({file_size} bytes)"
            );
            return Err(AutoRejectReason::FileTooLarge(file_size));
        }

        let request_id = Uuid::new_v4();
        let req = PendingRequest {
            request_id,
            peer_id,
            peer_name,
            file_name,
            file_size,
        };

        log::info!(
            "Song request from peer {}: \"{}\" ({} bytes) → id={}",
            req.peer_id,
            req.file_name,
            req.file_size,
            req.request_id,
        );

        self.pending_requests.push(req.clone());
        Ok(req)
    }

    // ── Accept / Reject ─────────────────────────────────────────────────

    /// Accept a pending request. Removes it from `pending_requests` and
    /// creates an `IncomingUpload` to receive chunks.
    ///
    /// Returns the `PendingRequest` that was accepted (for sending the
    /// `SongRequestAccepted` message), or `None` if the id was not found.
    pub fn accept(&mut self, request_id: Uuid) -> Option<PendingRequest> {
        let pos = self
            .pending_requests
            .iter()
            .position(|r| r.request_id == request_id)?;
        let req = self.pending_requests.remove(pos);

        log::info!("Accepted song request {request_id} from peer {}", req.peer_id);

        self.active_uploads.insert(
            request_id,
            IncomingUpload {
                request_id,
                file_name: req.file_name.clone(),
                peer_id: req.peer_id,
                expected_size: req.file_size,
                data: vec![0u8; req.file_size as usize],
                bytes_received: 0,
            },
        );

        Some(req)
    }

    /// Reject a pending request. Returns the removed `PendingRequest`,
    /// or `None` if not found.
    pub fn reject(&mut self, request_id: Uuid) -> Option<PendingRequest> {
        let pos = self
            .pending_requests
            .iter()
            .position(|r| r.request_id == request_id)?;
        let req = self.pending_requests.remove(pos);
        log::info!("Rejected song request {request_id} from peer {}", req.peer_id);
        Some(req)
    }

    // ── Upload flow ─────────────────────────────────────────────────────

    /// Receive a chunk of upload data.
    ///
    /// Returns `true` if the chunk was accepted. Returns `false` if there
    /// is no active upload for `request_id` or the chunk would overflow.
    pub fn receive_chunk(&mut self, request_id: Uuid, offset: u64, data: &[u8]) -> bool {
        let Some(upload) = self.active_uploads.get_mut(&request_id) else {
            log::warn!("Received chunk for unknown upload {request_id}");
            return false;
        };

        let start = offset as usize;
        let end = start + data.len();

        if end > upload.data.len() {
            log::warn!("Chunk overflows expected file size for upload {request_id}");
            return false;
        }

        upload.data[start..end].copy_from_slice(data);
        upload.bytes_received += data.len() as u64;

        log::debug!(
            "Upload {request_id}: {}/{} bytes ({:.1}%)",
            upload.bytes_received,
            upload.expected_size,
            if upload.expected_size > 0 {
                (upload.bytes_received as f64 / upload.expected_size as f64) * 100.0
            } else {
                100.0
            },
        );

        true
    }

    /// Finalize an upload after the peer sends `SongUploadComplete`.
    ///
    /// Verifies the SHA-256 hash against the provided value. Returns
    /// `Some(UploadResult)` with the outcome, or `None` if the upload
    /// was not found.
    pub fn complete_upload(
        &mut self,
        request_id: Uuid,
        expected_sha256: [u8; 32],
    ) -> Option<UploadResult> {
        let upload = self.active_uploads.remove(&request_id)?;

        let actual_sha256 = compute_sha256(&upload.data);

        if actual_sha256 == expected_sha256 {
            log::info!(
                "Upload {} complete: \"{}\" ({} bytes), hash verified",
                request_id,
                upload.file_name,
                upload.data.len(),
            );
            Some(UploadResult::Success {
                request_id,
                file_name: upload.file_name,
                peer_id: upload.peer_id,
                file_data: upload.data,
            })
        } else {
            log::error!(
                "Upload {} hash mismatch for \"{}\"",
                request_id,
                upload.file_name,
            );
            Some(UploadResult::HashMismatch { request_id })
        }
    }

    // ── Queries ─────────────────────────────────────────────────────────

    /// Get a snapshot of all pending requests (for the UI).
    pub fn pending_requests(&self) -> &[PendingRequest] {
        &self.pending_requests
    }

    /// Get the upload progress for a request (0.0–1.0).
    pub fn upload_progress(&self, request_id: Uuid) -> Option<f64> {
        let upload = self.active_uploads.get(&request_id)?;
        if upload.expected_size == 0 {
            return Some(1.0);
        }
        Some(upload.bytes_received as f64 / upload.expected_size as f64)
    }

    /// Check whether a request_id is in the active uploads.
    pub fn has_active_upload(&self, request_id: Uuid) -> bool {
        self.active_uploads.contains_key(&request_id)
    }

    // ── Cleanup ─────────────────────────────────────────────────────────

    /// Cancel any pending request or active upload from a specific peer
    /// (e.g. on disconnect).
    pub fn cancel_peer(&mut self, peer_id: u32) {
        let before_pending = self.pending_requests.len();
        self.pending_requests.retain(|r| r.peer_id != peer_id);
        let removed_pending = before_pending - self.pending_requests.len();

        let before_uploads = self.active_uploads.len();
        self.active_uploads.retain(|_, u| u.peer_id != peer_id);
        let removed_uploads = before_uploads - self.active_uploads.len();

        if removed_pending > 0 || removed_uploads > 0 {
            log::info!(
                "Cancelled {} pending request(s) and {} active upload(s) for peer {peer_id}",
                removed_pending,
                removed_uploads,
            );
        }
    }
}

impl Default for SongRequestManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Peer-side helpers ───────────────────────────────────────────────────────

/// Prepare the upload chunks for a file. Returns the list of
/// `(offset, chunk_data)` pairs and the SHA-256 hash.
///
/// This is a pure helper—the caller sends each chunk as a
/// `SongUploadChunk` message.
pub fn prepare_upload(file_data: &[u8]) -> (Vec<(u64, Vec<u8>)>, [u8; 32]) {
    let sha256 = compute_sha256(file_data);
    let chunks: Vec<(u64, Vec<u8>)> = file_data
        .chunks(UPLOAD_CHUNK_SIZE)
        .enumerate()
        .map(|(i, chunk)| ((i * UPLOAD_CHUNK_SIZE) as u64, chunk.to_vec()))
        .collect();
    (chunks, sha256)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_data(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i % 256) as u8).collect()
    }

    // ── Request lifecycle ───────────────────────────────────────────────

    #[test]
    fn test_receive_request_creates_pending() {
        let mut mgr = SongRequestManager::new();
        let req = mgr
            .receive_request(1, "Alice".into(), "song.mp3".into(), 1000)
            .unwrap();

        assert_eq!(req.peer_id, 1);
        assert_eq!(req.file_name, "song.mp3");
        assert_eq!(req.file_size, 1000);
        assert_eq!(mgr.pending_requests().len(), 1);
    }

    #[test]
    fn test_auto_reject_file_too_large() {
        let mut mgr = SongRequestManager::new();
        let result = mgr.receive_request(1, "Alice".into(), "big.mp3".into(), MAX_FILE_SIZE + 1);
        assert_eq!(
            result.unwrap_err(),
            AutoRejectReason::FileTooLarge(MAX_FILE_SIZE + 1)
        );
        assert!(mgr.pending_requests().is_empty());
    }

    #[test]
    fn test_accept_moves_to_upload() {
        let mut mgr = SongRequestManager::new();
        let req = mgr
            .receive_request(1, "Alice".into(), "song.mp3".into(), 1000)
            .unwrap();

        let accepted = mgr.accept(req.request_id).unwrap();
        assert_eq!(accepted.request_id, req.request_id);
        assert!(mgr.pending_requests().is_empty());
        assert!(mgr.has_active_upload(req.request_id));
    }

    #[test]
    fn test_reject_removes_pending() {
        let mut mgr = SongRequestManager::new();
        let req = mgr
            .receive_request(1, "Alice".into(), "song.mp3".into(), 1000)
            .unwrap();

        let rejected = mgr.reject(req.request_id).unwrap();
        assert_eq!(rejected.request_id, req.request_id);
        assert!(mgr.pending_requests().is_empty());
        assert!(!mgr.has_active_upload(req.request_id));
    }

    #[test]
    fn test_reject_unknown_returns_none() {
        let mut mgr = SongRequestManager::new();
        assert!(mgr.reject(Uuid::new_v4()).is_none());
    }

    #[test]
    fn test_accept_unknown_returns_none() {
        let mut mgr = SongRequestManager::new();
        assert!(mgr.accept(Uuid::new_v4()).is_none());
    }

    // ── Upload flow ─────────────────────────────────────────────────────

    #[test]
    fn test_full_upload_lifecycle_success() {
        let mut mgr = SongRequestManager::new();
        let data = make_test_data(200_000); // ~200 KB, multiple chunks
        let sha256 = compute_sha256(&data);

        let req = mgr
            .receive_request(1, "Alice".into(), "test.mp3".into(), data.len() as u64)
            .unwrap();
        mgr.accept(req.request_id);

        // Send data in chunks.
        let (chunks, _) = prepare_upload(&data);
        for (offset, chunk) in &chunks {
            assert!(mgr.receive_chunk(req.request_id, *offset, chunk));
        }

        // Complete.
        let result = mgr.complete_upload(req.request_id, sha256).unwrap();
        match result {
            UploadResult::Success {
                request_id,
                file_data,
                ..
            } => {
                assert_eq!(request_id, req.request_id);
                assert_eq!(file_data, data);
            }
            _ => panic!("Expected Success"),
        }

        // Upload should be cleaned up.
        assert!(!mgr.has_active_upload(req.request_id));
    }

    #[test]
    fn test_upload_hash_mismatch() {
        let mut mgr = SongRequestManager::new();
        let data = make_test_data(1000);
        let wrong_hash = [0xAA; 32];

        let req = mgr
            .receive_request(1, "Alice".into(), "test.mp3".into(), data.len() as u64)
            .unwrap();
        mgr.accept(req.request_id);

        // Upload single chunk.
        mgr.receive_chunk(req.request_id, 0, &data);

        let result = mgr.complete_upload(req.request_id, wrong_hash).unwrap();
        match result {
            UploadResult::HashMismatch { request_id } => {
                assert_eq!(request_id, req.request_id);
            }
            _ => panic!("Expected HashMismatch"),
        }
    }

    #[test]
    fn test_chunk_overflow_rejected() {
        let mut mgr = SongRequestManager::new();
        let req = mgr
            .receive_request(1, "Alice".into(), "test.mp3".into(), 100)
            .unwrap();
        mgr.accept(req.request_id);

        // Try to send a 200-byte chunk into a 100-byte buffer.
        let big_chunk = vec![0u8; 200];
        assert!(!mgr.receive_chunk(req.request_id, 0, &big_chunk));
    }

    #[test]
    fn test_chunk_for_unknown_upload_rejected() {
        let mut mgr = SongRequestManager::new();
        assert!(!mgr.receive_chunk(Uuid::new_v4(), 0, &[1, 2, 3]));
    }

    #[test]
    fn test_complete_unknown_returns_none() {
        let mut mgr = SongRequestManager::new();
        assert!(mgr.complete_upload(Uuid::new_v4(), [0; 32]).is_none());
    }

    // ── Upload progress ─────────────────────────────────────────────────

    #[test]
    fn test_upload_progress() {
        let mut mgr = SongRequestManager::new();
        let req = mgr
            .receive_request(1, "Alice".into(), "test.mp3".into(), 1000)
            .unwrap();
        mgr.accept(req.request_id);

        assert_eq!(mgr.upload_progress(req.request_id), Some(0.0));

        mgr.receive_chunk(req.request_id, 0, &[0u8; 500]);
        assert!((mgr.upload_progress(req.request_id).unwrap() - 0.5).abs() < 0.001);

        mgr.receive_chunk(req.request_id, 500, &[0u8; 500]);
        assert!((mgr.upload_progress(req.request_id).unwrap() - 1.0).abs() < 0.001);
    }

    // ── Cancel peer ─────────────────────────────────────────────────────

    #[test]
    fn test_cancel_peer_removes_pending_and_uploads() {
        let mut mgr = SongRequestManager::new();

        // One pending request from peer 1.
        let req1 = mgr
            .receive_request(1, "Alice".into(), "a.mp3".into(), 100)
            .unwrap();
        // One accepted (active upload) from peer 1.
        let req2 = mgr
            .receive_request(1, "Alice".into(), "b.mp3".into(), 200)
            .unwrap();
        mgr.accept(req2.request_id);

        // One pending from peer 2 — should survive.
        let req3 = mgr
            .receive_request(2, "Bob".into(), "c.mp3".into(), 300)
            .unwrap();

        mgr.cancel_peer(1);

        assert!(mgr.pending_requests().len() == 1);
        assert_eq!(mgr.pending_requests()[0].request_id, req3.request_id);
        assert!(!mgr.has_active_upload(req2.request_id));
        // req1 was pending and should be gone.
        assert!(mgr.reject(req1.request_id).is_none());
    }

    // ── Peer-side prepare_upload ────────────────────────────────────────

    #[test]
    fn test_prepare_upload_chunks_and_hash() {
        let data = make_test_data(UPLOAD_CHUNK_SIZE * 2 + 100); // 2 full + partial
        let (chunks, sha256) = prepare_upload(&data);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, 0);
        assert_eq!(chunks[0].1.len(), UPLOAD_CHUNK_SIZE);
        assert_eq!(chunks[1].0, UPLOAD_CHUNK_SIZE as u64);
        assert_eq!(chunks[1].1.len(), UPLOAD_CHUNK_SIZE);
        assert_eq!(chunks[2].0, (UPLOAD_CHUNK_SIZE * 2) as u64);
        assert_eq!(chunks[2].1.len(), 100);

        // Reassemble and verify hash.
        let mut reassembled = Vec::new();
        for (_, chunk) in &chunks {
            reassembled.extend_from_slice(chunk);
        }
        assert_eq!(compute_sha256(&reassembled), sha256);
    }

    #[test]
    fn test_prepare_upload_exact_chunk_boundary() {
        let data = make_test_data(UPLOAD_CHUNK_SIZE * 3);
        let (chunks, _) = prepare_upload(&data);
        assert_eq!(chunks.len(), 3);
        for (i, (offset, chunk)) in chunks.iter().enumerate() {
            assert_eq!(*offset, (i * UPLOAD_CHUNK_SIZE) as u64);
            assert_eq!(chunk.len(), UPLOAD_CHUNK_SIZE);
        }
    }

    // ── Multiple requests ───────────────────────────────────────────────

    #[test]
    fn test_multiple_pending_requests() {
        let mut mgr = SongRequestManager::new();
        let r1 = mgr
            .receive_request(1, "Alice".into(), "a.mp3".into(), 100)
            .unwrap();
        let r2 = mgr
            .receive_request(2, "Bob".into(), "b.mp3".into(), 200)
            .unwrap();
        let r3 = mgr
            .receive_request(3, "Carol".into(), "c.mp3".into(), 300)
            .unwrap();

        assert_eq!(mgr.pending_requests().len(), 3);

        // Reject r2.
        mgr.reject(r2.request_id);
        assert_eq!(mgr.pending_requests().len(), 2);

        // Accept r1.
        mgr.accept(r1.request_id);
        assert_eq!(mgr.pending_requests().len(), 1);
        assert_eq!(mgr.pending_requests()[0].request_id, r3.request_id);
    }

    #[test]
    fn test_file_at_max_size_accepted() {
        let mut mgr = SongRequestManager::new();
        let result = mgr.receive_request(1, "Alice".into(), "big.mp3".into(), MAX_FILE_SIZE);
        assert!(result.is_ok());
    }
}
