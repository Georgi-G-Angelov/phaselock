use crate::network::messages::Message;
use crate::network::tcp::TcpHost;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

// ── Constants ───────────────────────────────────────────────────────────────

/// Chunk size for file transfers: 64 KB.
const CHUNK_SIZE: usize = 64 * 1024;

/// Maximum file size: 50 MB.
const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;

/// Maximum retries per peer on hash mismatch.
const MAX_RETRIES: u8 = 3;

// ── TransferEvent ───────────────────────────────────────────────────────────

/// Events emitted by the transfer layer.
#[derive(Debug, Clone)]
pub enum TransferEvent {
    /// Progress update for sending a file to a peer (0.0–1.0).
    SendProgress {
        file_id: Uuid,
        peer_id: u32,
        progress: f64,
    },
    /// A peer confirmed successful receipt.
    PeerConfirmed {
        file_id: Uuid,
        peer_id: u32,
    },
    /// A peer's transfer failed after max retries.
    PeerFailed {
        file_id: Uuid,
        peer_id: u32,
    },
    /// All peers confirmed receipt of a file.
    AllConfirmed {
        file_id: Uuid,
    },
    /// Receive progress for an incoming file (0.0–1.0).
    ReceiveProgress {
        file_id: Uuid,
        progress: f64,
    },
    /// A file was fully received and hash verified.
    FileReady {
        file_id: Uuid,
        file_name: String,
    },
    /// An incoming file failed hash verification.
    FileCorrupted {
        file_id: Uuid,
    },
}

// ── TransferError ───────────────────────────────────────────────────────────

/// Errors that can occur during file transfer operations.
#[derive(Debug)]
pub enum TransferError {
    Io(std::io::Error),
    FileTooLarge(u64),
    FileEmpty,
    AlreadyTransferring(Uuid),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::FileTooLarge(size) => write!(f, "file too large: {size} bytes (max {MAX_FILE_SIZE})"),
            Self::FileEmpty => write!(f, "file is empty"),
            Self::AlreadyTransferring(id) => write!(f, "file {id} is already being transferred"),
        }
    }
}

impl std::error::Error for TransferError {}

// ── Host side: FileTransferManager ──────────────────────────────────────────

/// State for one outgoing file transfer.
struct TransferState {
    file_id: Uuid,
    file_name: String,
    file_data: Vec<u8>,
    sha256: [u8; 32],
    peers_pending: HashSet<u32>,
    peers_confirmed: HashSet<u32>,
    peers_failed: HashSet<u32>,
    retry_counts: HashMap<u32, u8>,
}

/// Host-side file transfer manager. Sends files to peers in chunks.
pub struct FileTransferManager {
    active_transfers: HashMap<Uuid, TransferState>,
}

impl FileTransferManager {
    pub fn new() -> Self {
        Self {
            active_transfers: HashMap::new(),
        }
    }

    /// Start transferring a file to a set of peers.
    ///
    /// Reads the file, computes SHA-256, sends `FileTransferStart` + chunks.
    /// Returns the generated `file_id`.
    pub async fn start_transfer(
        &mut self,
        file_path: &Path,
        peer_ids: &[u32],
        tcp_host: &Arc<TcpHost>,
    ) -> Result<Uuid, TransferError> {
        let file_data = tokio::fs::read(file_path)
            .await
            .map_err(TransferError::Io)?;

        self.start_transfer_from_bytes(
            file_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            file_data,
            peer_ids,
            tcp_host,
        )
        .await
    }

    /// Start transferring raw bytes (useful for tests and peer-to-host uploads).
    pub async fn start_transfer_from_bytes(
        &mut self,
        file_name: String,
        file_data: Vec<u8>,
        peer_ids: &[u32],
        tcp_host: &Arc<TcpHost>,
    ) -> Result<Uuid, TransferError> {
        if file_data.is_empty() {
            return Err(TransferError::FileEmpty);
        }
        if file_data.len() as u64 > MAX_FILE_SIZE {
            return Err(TransferError::FileTooLarge(file_data.len() as u64));
        }

        let sha256 = compute_sha256(&file_data);
        let file_id = Uuid::new_v4();

        if self.active_transfers.contains_key(&file_id) {
            return Err(TransferError::AlreadyTransferring(file_id));
        }

        let peers_pending: HashSet<u32> = peer_ids.iter().copied().collect();

        log::info!(
            "Starting transfer of \"{}\" ({} bytes, {}) to {} peer(s)",
            file_name,
            file_data.len(),
            file_id,
            peers_pending.len()
        );

        // Send FileTransferStart to each peer.
        let start_msg = Message::FileTransferStart {
            file_id,
            file_name: file_name.clone(),
            size: file_data.len() as u64,
            sha256,
        };
        for &pid in peer_ids {
            tcp_host.send_to_peer(pid, &start_msg).await;
        }

        // Send chunks to all peers.
        Self::send_chunks(tcp_host, peer_ids, file_id, &file_data).await;

        self.active_transfers.insert(
            file_id,
            TransferState {
                file_id,
                file_name,
                file_data,
                sha256,
                peers_pending,
                peers_confirmed: HashSet::new(),
                peers_failed: HashSet::new(),
                retry_counts: HashMap::new(),
            },
        );

        Ok(file_id)
    }

    /// Send file data in 64 KB chunks to the given peers.
    async fn send_chunks(
        tcp_host: &Arc<TcpHost>,
        peer_ids: &[u32],
        file_id: Uuid,
        data: &[u8],
    ) {
        let total = data.len();
        let mut offset: u64 = 0;

        for chunk in data.chunks(CHUNK_SIZE) {
            let msg = Message::FileChunk {
                file_id,
                offset,
                data: chunk.to_vec(),
            };

            for &pid in peer_ids {
                tcp_host.send_to_peer(pid, &msg).await;
            }

            offset += chunk.len() as u64;
            log::debug!(
                "Sent chunk offset={} len={} ({:.1}%)",
                offset - chunk.len() as u64,
                chunk.len(),
                (offset as f64 / total as f64) * 100.0
            );
        }
    }

    /// Re-send a file to a specific peer (on hash mismatch retry).
    async fn resend_to_peer(
        tcp_host: &Arc<TcpHost>,
        peer_id: u32,
        state: &TransferState,
    ) {
        let start_msg = Message::FileTransferStart {
            file_id: state.file_id,
            file_name: state.file_name.clone(),
            size: state.file_data.len() as u64,
            sha256: state.sha256,
        };
        tcp_host.send_to_peer(peer_id, &start_msg).await;
        Self::send_chunks(tcp_host, &[peer_id], state.file_id, &state.file_data).await;
    }

    /// Handle a `FileReceived` message from a peer.
    ///
    /// Returns a `TransferEvent` describing what happened.
    pub async fn handle_file_received(
        &mut self,
        peer_id: u32,
        file_id: Uuid,
        hash_ok: bool,
        tcp_host: &Arc<TcpHost>,
    ) -> Option<TransferEvent> {
        let state = self.active_transfers.get_mut(&file_id)?;

        if hash_ok {
            state.peers_pending.remove(&peer_id);
            state.peers_confirmed.insert(peer_id);
            log::info!("Peer {peer_id} confirmed file {file_id}");

            if state.peers_pending.is_empty() && state.peers_failed.is_empty() {
                log::info!("All peers confirmed file {file_id}");
                return Some(TransferEvent::AllConfirmed { file_id });
            }
            Some(TransferEvent::PeerConfirmed { file_id, peer_id })
        } else {
            let retries = state.retry_counts.entry(peer_id).or_insert(0);
            *retries += 1;
            let count = *retries;

            if count < MAX_RETRIES {
                log::warn!(
                    "Peer {peer_id} hash mismatch for {file_id}, retry {count}/{MAX_RETRIES}"
                );
                Self::resend_to_peer(tcp_host, peer_id, state).await;
                None
            } else {
                log::error!(
                    "Peer {peer_id} failed to receive {file_id} after {MAX_RETRIES} retries"
                );
                state.peers_pending.remove(&peer_id);
                state.peers_failed.insert(peer_id);

                if state.peers_pending.is_empty() {
                    // All peers either confirmed or failed.
                    if state.peers_failed.is_empty() {
                        Some(TransferEvent::AllConfirmed { file_id })
                    } else {
                        Some(TransferEvent::PeerFailed { file_id, peer_id })
                    }
                } else {
                    Some(TransferEvent::PeerFailed { file_id, peer_id })
                }
            }
        }
    }

    /// Check if all peers have confirmed receipt of the file.
    pub fn is_all_ready(&self, file_id: Uuid) -> bool {
        match self.active_transfers.get(&file_id) {
            Some(state) => state.peers_pending.is_empty() && state.peers_failed.is_empty(),
            None => false,
        }
    }

    /// Cancel a peer's part of any active transfers (e.g. on disconnect).
    pub fn cancel_peer(&mut self, peer_id: u32) {
        for state in self.active_transfers.values_mut() {
            state.peers_pending.remove(&peer_id);
        }
    }

    /// Get send progress for a file (fraction of peers confirmed).
    pub fn send_progress(&self, file_id: Uuid) -> Option<f64> {
        let state = self.active_transfers.get(&file_id)?;
        let total = state.peers_confirmed.len() + state.peers_pending.len() + state.peers_failed.len();
        if total == 0 {
            return Some(1.0);
        }
        Some(state.peers_confirmed.len() as f64 / total as f64)
    }

    /// Clean up a completed transfer, freeing its data.
    pub fn remove_transfer(&mut self, file_id: Uuid) {
        self.active_transfers.remove(&file_id);
    }
}

impl Default for FileTransferManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Peer side: FileReceiver ─────────────────────────────────────────────────

/// State for one incoming file transfer.
struct IncomingFile {
    file_id: Uuid,
    file_name: String,
    expected_size: u64,
    expected_sha256: [u8; 32],
    data: Vec<u8>,
    bytes_received: u64,
}

/// Peer-side file receiver. Reassembles chunks and verifies integrity.
pub struct FileReceiver {
    incoming: HashMap<Uuid, IncomingFile>,
    completed_files: HashMap<Uuid, Vec<u8>>,
}

impl FileReceiver {
    pub fn new() -> Self {
        Self {
            incoming: HashMap::new(),
            completed_files: HashMap::new(),
        }
    }

    /// Handle a `FileTransferStart` message. Prepares to receive chunks.
    pub fn handle_transfer_start(
        &mut self,
        file_id: Uuid,
        file_name: String,
        size: u64,
        sha256: [u8; 32],
    ) -> Result<(), TransferError> {
        if self.completed_files.contains_key(&file_id) {
            // Already have this file — ignore duplicate.
            log::debug!("Ignoring duplicate transfer start for {file_id}");
            return Ok(());
        }

        if size == 0 {
            return Err(TransferError::FileEmpty);
        }
        if size > MAX_FILE_SIZE {
            return Err(TransferError::FileTooLarge(size));
        }

        log::info!("Receiving file \"{file_name}\" ({size} bytes, {file_id})");

        // Pre-allocate buffer.
        let data = vec![0u8; size as usize];

        self.incoming.insert(
            file_id,
            IncomingFile {
                file_id,
                file_name,
                expected_size: size,
                expected_sha256: sha256,
                data,
                bytes_received: 0,
            },
        );

        Ok(())
    }

    /// Handle a `FileChunk` message. Returns a `TransferEvent` if the file
    /// is now complete (either `FileReady` or `FileCorrupted`), plus the
    /// `Message::FileReceived` to send back to the host.
    pub fn handle_chunk(
        &mut self,
        file_id: Uuid,
        offset: u64,
        chunk_data: &[u8],
    ) -> Option<(TransferEvent, Message)> {
        let incoming = self.incoming.get_mut(&file_id)?;

        let start = offset as usize;
        let end = start + chunk_data.len();

        if end > incoming.data.len() {
            log::warn!("Chunk overflows expected file size for {file_id}");
            return None;
        }

        incoming.data[start..end].copy_from_slice(chunk_data);
        incoming.bytes_received += chunk_data.len() as u64;

        let pct = if incoming.expected_size > 0 {
            (incoming.bytes_received as f64 / incoming.expected_size as f64) * 100.0
        } else {
            100.0
        };
        log::debug!(
            "File {file_id}: received {}/{} bytes ({pct:.1}%)",
            incoming.bytes_received,
            incoming.expected_size,
        );

        // Check if the file is fully received.
        if incoming.bytes_received >= incoming.expected_size {
            let incoming = self.incoming.remove(&file_id).unwrap();
            let actual_sha256 = compute_sha256(&incoming.data);
            let hash_ok = actual_sha256 == incoming.expected_sha256;

            let response = Message::FileReceived { file_id, hash_ok };

            if hash_ok {
                log::info!(
                    "File \"{}\" ({file_id}) received and verified",
                    incoming.file_name
                );
                self.completed_files.insert(file_id, incoming.data);
                Some((
                    TransferEvent::FileReady {
                        file_id,
                        file_name: incoming.file_name,
                    },
                    response,
                ))
            } else {
                log::warn!(
                    "File \"{}\" ({file_id}) hash mismatch!",
                    incoming.file_name
                );
                Some((TransferEvent::FileCorrupted { file_id }, response))
            }
        } else {
            None
        }
    }

    /// Get receive progress for an incoming file (0.0–1.0).
    pub fn receive_progress(&self, file_id: Uuid) -> Option<f64> {
        let incoming = self.incoming.get(&file_id)?;
        if incoming.expected_size == 0 {
            return Some(1.0);
        }
        Some(incoming.bytes_received as f64 / incoming.expected_size as f64)
    }

    /// Get a completed file's data.
    pub fn get_file(&self, file_id: Uuid) -> Option<&Vec<u8>> {
        self.completed_files.get(&file_id)
    }

    /// Check if a file has been successfully received.
    pub fn has_file(&self, file_id: Uuid) -> bool {
        self.completed_files.contains_key(&file_id)
    }

    /// Remove a completed file from memory (e.g. when no longer needed).
    pub fn remove_file(&mut self, file_id: Uuid) {
        self.completed_files.remove(&file_id);
    }
}

impl Default for FileReceiver {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Compute the SHA-256 hash of a byte slice.
pub fn compute_sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&result);
    hash
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::tcp::{TcpEvent, TcpHost};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    const TEST_TIMEOUT: Duration = Duration::from_secs(15);

    async fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    }

    // ── Unit test: SHA-256 correctness ──────────────────────────────────

    #[test]
    fn test_sha256_known_value() {
        // SHA-256 of the empty string is a well-known value, but our code
        // rejects empty files. Test with "hello":
        // echo -n "hello" | sha256sum => 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let hash = compute_sha256(b"hello");
        let expected: [u8; 32] = [
            0x2c, 0xf2, 0x4d, 0xba, 0x5f, 0xb0, 0xa3, 0x0e,
            0x26, 0xe8, 0x3b, 0x2a, 0xc5, 0xb9, 0xe2, 0x9e,
            0x1b, 0x16, 0x1e, 0x5c, 0x1f, 0xa7, 0x42, 0x5e,
            0x73, 0x04, 0x33, 0x62, 0x93, 0x8b, 0x98, 0x24,
        ];
        assert_eq!(hash, expected);
    }

    // ── Unit test: chunk and reassemble ─────────────────────────────────

    #[test]
    fn test_chunk_and_reassemble() {
        // Create a 200 KB data block.
        let original: Vec<u8> = (0u8..=255).cycle().take(200 * 1024).collect();
        let original_hash = compute_sha256(&original);

        // Chunk it.
        let chunks: Vec<(u64, Vec<u8>)> = original
            .chunks(CHUNK_SIZE)
            .enumerate()
            .map(|(i, c)| ((i * CHUNK_SIZE) as u64, c.to_vec()))
            .collect();

        // Should be 4 chunks: 64 + 64 + 64 + 8 KB.
        assert_eq!(chunks.len(), 4);

        // Reassemble.
        let mut reassembled = vec![0u8; original.len()];
        for (offset, data) in &chunks {
            let start = *offset as usize;
            reassembled[start..start + data.len()].copy_from_slice(data);
        }

        assert_eq!(reassembled, original);
        assert_eq!(compute_sha256(&reassembled), original_hash);
    }

    // ── Unit test: FileReceiver reassembly ──────────────────────────────

    #[test]
    fn test_receiver_reassembly() {
        let original: Vec<u8> = (0u8..=255).cycle().take(150_000).collect();
        let sha256 = compute_sha256(&original);
        let file_id = Uuid::new_v4();

        let mut receiver = FileReceiver::new();
        receiver
            .handle_transfer_start(file_id, "test.mp3".into(), original.len() as u64, sha256)
            .unwrap();

        // Feed chunks.
        let mut last_result = None;
        for chunk in original.chunks(CHUNK_SIZE) {
            let offset = if let Some(inc) = receiver.incoming.get(&file_id) {
                inc.bytes_received
            } else {
                break;
            };
            last_result = receiver.handle_chunk(file_id, offset, chunk);
        }

        // Last chunk should trigger completion.
        let (event, msg) = last_result.expect("should have completed");
        assert!(matches!(event, TransferEvent::FileReady { .. }));
        assert!(matches!(msg, Message::FileReceived { hash_ok: true, .. }));

        // Verify data.
        assert!(receiver.has_file(file_id));
        assert_eq!(receiver.get_file(file_id).unwrap(), &original);
    }

    // ── Unit test: hash mismatch detection ──────────────────────────────

    #[test]
    fn test_receiver_hash_mismatch() {
        let original: Vec<u8> = vec![42u8; 1000];
        let bad_sha256 = [0xFFu8; 32]; // Wrong hash on purpose.
        let file_id = Uuid::new_v4();

        let mut receiver = FileReceiver::new();
        receiver
            .handle_transfer_start(file_id, "bad.mp3".into(), 1000, bad_sha256)
            .unwrap();

        let result = receiver.handle_chunk(file_id, 0, &original);
        let (event, msg) = result.expect("should have completed");
        assert!(matches!(event, TransferEvent::FileCorrupted { .. }));
        assert!(matches!(msg, Message::FileReceived { hash_ok: false, .. }));
        assert!(!receiver.has_file(file_id));
    }

    // ── Unit test: retry logic ──────────────────────────────────────────

    #[tokio::test]
    async fn test_retry_on_hash_mismatch() {
        let (tcp_tx, _tcp_rx) = mpsc::channel::<TcpEvent>(128);
        let port = free_port().await;
        let tcp_host = Arc::new(TcpHost::start(port, tcp_tx).await.unwrap());

        let mut mgr = FileTransferManager::new();
        let file_id = Uuid::new_v4();

        // Manually insert a transfer state.
        let mut peers_pending = HashSet::new();
        peers_pending.insert(1);
        mgr.active_transfers.insert(
            file_id,
            TransferState {
                file_id,
                file_name: "test.mp3".into(),
                file_data: vec![42u8; 100],
                sha256: compute_sha256(&vec![42u8; 100]),
                peers_pending,
                peers_confirmed: HashSet::new(),
                peers_failed: HashSet::new(),
                retry_counts: HashMap::new(),
            },
        );

        // First failed attempt — should retry (return None = retrying).
        let event = mgr.handle_file_received(1, file_id, false, &tcp_host).await;
        assert!(event.is_none(), "Should be retrying silently");
        assert_eq!(*mgr.active_transfers[&file_id].retry_counts.get(&1).unwrap(), 1);

        // Second failed attempt — still retrying.
        let event = mgr.handle_file_received(1, file_id, false, &tcp_host).await;
        assert!(event.is_none());
        assert_eq!(*mgr.active_transfers[&file_id].retry_counts.get(&1).unwrap(), 2);

        // Third failed attempt — should mark as failed.
        let event = mgr.handle_file_received(1, file_id, false, &tcp_host).await;
        assert!(matches!(event, Some(TransferEvent::PeerFailed { .. })));
        assert!(mgr.active_transfers[&file_id].peers_failed.contains(&1));
    }

    // ── Unit test: all-ready check ──────────────────────────────────────

    #[tokio::test]
    async fn test_is_all_ready() {
        let (tcp_tx, _tcp_rx) = mpsc::channel::<TcpEvent>(128);
        let port = free_port().await;
        let tcp_host = Arc::new(TcpHost::start(port, tcp_tx).await.unwrap());

        let mut mgr = FileTransferManager::new();
        let file_id = Uuid::new_v4();

        let mut peers_pending = HashSet::new();
        peers_pending.insert(1);
        peers_pending.insert(2);
        mgr.active_transfers.insert(
            file_id,
            TransferState {
                file_id,
                file_name: "test.mp3".into(),
                file_data: vec![42u8; 100],
                sha256: compute_sha256(&vec![42u8; 100]),
                peers_pending,
                peers_confirmed: HashSet::new(),
                peers_failed: HashSet::new(),
                retry_counts: HashMap::new(),
            },
        );

        assert!(!mgr.is_all_ready(file_id));

        // Peer 1 confirms.
        let event = mgr.handle_file_received(1, file_id, true, &tcp_host).await;
        assert!(matches!(event, Some(TransferEvent::PeerConfirmed { .. })));
        assert!(!mgr.is_all_ready(file_id));

        // Peer 2 confirms.
        let event = mgr.handle_file_received(2, file_id, true, &tcp_host).await;
        assert!(matches!(event, Some(TransferEvent::AllConfirmed { .. })));
        assert!(mgr.is_all_ready(file_id));
    }

    // ── Unit test: empty file rejected ──────────────────────────────────

    #[test]
    fn test_empty_file_rejected() {
        let mut receiver = FileReceiver::new();
        let result = receiver.handle_transfer_start(Uuid::new_v4(), "empty.mp3".into(), 0, [0; 32]);
        assert!(matches!(result, Err(TransferError::FileEmpty)));
    }

    // ── Unit test: oversized file rejected ──────────────────────────────

    #[test]
    fn test_oversized_file_rejected() {
        let mut receiver = FileReceiver::new();
        let result = receiver.handle_transfer_start(
            Uuid::new_v4(),
            "huge.mp3".into(),
            MAX_FILE_SIZE + 1,
            [0; 32],
        );
        assert!(matches!(result, Err(TransferError::FileTooLarge(_))));
    }

    // ── Unit test: duplicate file_id ignored ────────────────────────────

    #[test]
    fn test_duplicate_file_id_ignored() {
        let original = vec![42u8; 1000];
        let sha256 = compute_sha256(&original);
        let file_id = Uuid::new_v4();

        let mut receiver = FileReceiver::new();

        // First: receive the file.
        receiver
            .handle_transfer_start(file_id, "test.mp3".into(), 1000, sha256)
            .unwrap();
        let _ = receiver.handle_chunk(file_id, 0, &original);
        assert!(receiver.has_file(file_id));

        // Second: duplicate start should be ignored.
        let result = receiver.handle_transfer_start(file_id, "test.mp3".into(), 1000, sha256);
        assert!(result.is_ok()); // No error, just silently ignored.
    }

    // ── Unit test: progress tracking ────────────────────────────────────

    #[test]
    fn test_receive_progress() {
        let data = vec![0u8; CHUNK_SIZE * 4]; // Exactly 4 chunks.
        let sha256 = compute_sha256(&data);
        let file_id = Uuid::new_v4();

        let mut receiver = FileReceiver::new();
        receiver
            .handle_transfer_start(file_id, "prog.mp3".into(), data.len() as u64, sha256)
            .unwrap();

        // Before any chunks.
        assert_eq!(receiver.receive_progress(file_id), Some(0.0));

        // After 1 chunk (25%).
        receiver.handle_chunk(file_id, 0, &data[..CHUNK_SIZE]);
        let prog = receiver.receive_progress(file_id).unwrap();
        assert!((prog - 0.25).abs() < 0.001);

        // After 2 chunks (50%).
        receiver.handle_chunk(file_id, CHUNK_SIZE as u64, &data[CHUNK_SIZE..CHUNK_SIZE * 2]);
        let prog = receiver.receive_progress(file_id).unwrap();
        assert!((prog - 0.50).abs() < 0.001);
    }

    // ── Integration test: host → peer file transfer over TCP ────────────

    #[tokio::test]
    async fn test_host_to_peer_transfer_over_tcp() {
        use crate::network::tcp::TcpPeer;
        use std::net::SocketAddr;

        let _ = timeout(TEST_TIMEOUT, async {
            let port = free_port().await;
            let (host_tx, mut host_rx) = mpsc::channel::<TcpEvent>(128);
            let tcp_host = Arc::new(TcpHost::start(port, host_tx).await.unwrap());

            // Connect a peer.
            let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let (peer_tx, mut peer_rx) = mpsc::channel::<TcpEvent>(128);
            let mut tcp_peer = TcpPeer::connect(addr, peer_tx, false).await.unwrap();

            // Wait for PeerConnected on the host side.
            let peer_id = loop {
                if let Some(TcpEvent::PeerConnected { peer_id, .. }) = host_rx.recv().await {
                    break peer_id;
                }
            };

            // Create test data (~100 KB, enough for 2 chunks).
            let test_data: Vec<u8> = (0u8..=255).cycle().take(100_000).collect();
            let sha256 = compute_sha256(&test_data);

            // Host: start the transfer.
            let mut mgr = FileTransferManager::new();
            let file_id = mgr
                .start_transfer_from_bytes(
                    "test_song.mp3".into(),
                    test_data.clone(),
                    &[peer_id],
                    &tcp_host,
                )
                .await
                .unwrap();

            // Peer: receive messages and feed to FileReceiver.
            let mut receiver = FileReceiver::new();
            let mut got_file_ready = false;

            for _ in 0..50 {
                match timeout(Duration::from_millis(500), peer_rx.recv()).await {
                    Ok(Some(TcpEvent::MessageReceived { message, .. })) => match message {
                        Message::FileTransferStart {
                            file_id: fid,
                            file_name,
                            size,
                            sha256,
                        } => {
                            receiver
                                .handle_transfer_start(fid, file_name, size, sha256)
                                .unwrap();
                        }
                        Message::FileChunk {
                            file_id: fid,
                            offset,
                            data,
                        } => {
                            if let Some((event, response)) =
                                receiver.handle_chunk(fid, offset, &data)
                            {
                                // Send FileReceived back to host.
                                tcp_peer.send(&response).await.unwrap();

                                if matches!(event, TransferEvent::FileReady { .. }) {
                                    got_file_ready = true;
                                    break;
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => break,
                }
            }

            assert!(got_file_ready, "Peer should have received the complete file");
            assert!(receiver.has_file(file_id));
            assert_eq!(receiver.get_file(file_id).unwrap(), &test_data);
            assert_eq!(compute_sha256(receiver.get_file(file_id).unwrap()), sha256);

            // Host: receive the FileReceived confirmation.
            for _ in 0..10 {
                match timeout(Duration::from_millis(500), host_rx.recv()).await {
                    Ok(Some(TcpEvent::MessageReceived {
                        peer_id: pid,
                        message: Message::FileReceived { file_id: fid, hash_ok },
                    })) => {
                        let event = mgr
                            .handle_file_received(pid, fid, hash_ok, &tcp_host)
                            .await;
                        assert!(matches!(event, Some(TransferEvent::AllConfirmed { .. })));
                        break;
                    }
                    _ => continue,
                }
            }

            assert!(mgr.is_all_ready(file_id));
        })
        .await
        .unwrap();
    }
}
