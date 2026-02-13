use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

// ── TCP Messages ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Message {
    // Session management
    JoinRequest { display_name: String },
    JoinAccepted { peer_id: u32, session_state: SessionState },
    JoinRejected { reason: String },
    LeaveSession { peer_id: u32 },
    SessionEnd,
    Heartbeat,

    // File transfer
    FileTransferStart { file_id: Uuid, file_name: String, size: u64, sha256: [u8; 32] },
    FileChunk { file_id: Uuid, offset: u64, data: Vec<u8> },
    FileReceived { file_id: Uuid, hash_ok: bool },

    // Song requests
    SongRequest { file_name: String, file_size: u64 },
    SongRequestAccepted { request_id: Uuid },
    SongRequestRejected { request_id: Uuid },
    SongUploadChunk { request_id: Uuid, offset: u64, data: Vec<u8> },
    SongUploadComplete { request_id: Uuid, sha256: [u8; 32] },

    // Playback control
    PlayCommand { file_id: Uuid, position_samples: u64, target_time_ns: u64 },
    PauseCommand { position_samples: u64 },
    ResumeCommand { position_samples: u64, target_time_ns: u64 },
    StopCommand,
    SeekCommand { position_samples: u64, target_time_ns: u64 },

    // Queue sync
    QueueUpdate { queue: Vec<QueueItem> },
}

// ── Supporting Structs ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    pub session_name: String,
    pub host_name: String,
    pub peers: Vec<PeerInfo>,
    pub queue: Vec<QueueItem>,
    pub current_track: Option<CurrentTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerInfo {
    pub peer_id: u32,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueueItem {
    pub id: Uuid,
    pub file_name: String,
    pub duration_secs: f64,
    pub added_by: String,
    pub status: QueueItemStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum QueueItemStatus {
    Transferring,
    Ready,
    Playing,
    Played,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CurrentTrack {
    pub file_id: Uuid,
    pub file_name: String,
    pub position_samples: u64,
    pub sample_rate: u32,
    pub is_playing: bool,
}

// ── UDP Clock Sync Messages ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClockMessage {
    ClockPing { peer_id: u32, peer_send_time_ns: u64 },
    ClockPong {
        peer_id: u32,
        peer_send_time_ns: u64,
        host_recv_time_ns: u64,
        host_send_time_ns: u64,
    },
}

// ── Message Framing ─────────────────────────────────────────────────────────
//
// Wire format: [length: u32 LE][bincode payload]
//
// Used for TCP transport. UDP clock messages are small enough to fit in a
// single datagram and don't need framing.

/// Maximum message size: 16 MB. Prevents allocation bombs from malformed data.
const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

/// Serialize `msg` with bincode, write a little-endian u32 length prefix,
/// then the payload bytes.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> Result<(), FrameError> {
    let payload = bincode::serialize(msg).map_err(FrameError::Serialize)?;
    let len = payload.len() as u32;
    writer.write_all(&len.to_le_bytes()).await.map_err(FrameError::Io)?;
    writer.write_all(&payload).await.map_err(FrameError::Io)?;
    writer.flush().await.map_err(FrameError::Io)?;
    Ok(())
}

/// Read a length-prefixed message from the stream. Returns the deserialized
/// `Message`, or an error if the stream is closed / data is malformed.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Message, FrameError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.map_err(FrameError::Io)?;
    let len = u32::from_le_bytes(len_buf);

    if len > MAX_MESSAGE_SIZE {
        return Err(FrameError::MessageTooLarge(len));
    }

    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await.map_err(FrameError::Io)?;
    bincode::deserialize(&payload).map_err(FrameError::Deserialize)
}

/// Errors that can occur during message framing.
#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    Serialize(bincode::Error),
    Deserialize(bincode::Error),
    MessageTooLarge(u32),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::Serialize(e) => write!(f, "serialization error: {e}"),
            Self::Deserialize(e) => write!(f, "deserialization error: {e}"),
            Self::MessageTooLarge(sz) => write!(f, "message too large: {sz} bytes"),
        }
    }
}

impl std::error::Error for FrameError {}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Helper: round-trip a Message through bincode serialize/deserialize.
    fn roundtrip_message(msg: &Message) {
        let bytes = bincode::serialize(msg).expect("serialize");
        let decoded: Message = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(*msg, decoded);
    }

    /// Helper: round-trip a ClockMessage through bincode.
    fn roundtrip_clock(msg: &ClockMessage) {
        let bytes = bincode::serialize(msg).expect("serialize");
        let decoded: ClockMessage = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(*msg, decoded);
    }

    /// Helper: round-trip a Message through the async framing layer.
    async fn roundtrip_framed(msg: &Message) {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, msg).await.expect("write");
        let mut cursor = Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.expect("read");
        assert_eq!(*msg, decoded);
    }

    // ── Session messages ────────────────────────────────────────────────

    #[test]
    fn test_join_request() {
        roundtrip_message(&Message::JoinRequest {
            display_name: "Alice".into(),
        });
    }

    #[test]
    fn test_join_accepted() {
        roundtrip_message(&Message::JoinAccepted {
            peer_id: 1,
            session_state: SessionState {
                session_name: "Chill Vibes".into(),
                host_name: "Bob".into(),
                peers: vec![PeerInfo { peer_id: 1, display_name: "Alice".into() }],
                queue: vec![],
                current_track: None,
            },
        });
    }

    #[test]
    fn test_join_rejected() {
        roundtrip_message(&Message::JoinRejected {
            reason: "Session full".into(),
        });
    }

    #[test]
    fn test_leave_session() {
        roundtrip_message(&Message::LeaveSession { peer_id: 3 });
    }

    #[test]
    fn test_session_end() {
        roundtrip_message(&Message::SessionEnd);
    }

    #[test]
    fn test_heartbeat() {
        roundtrip_message(&Message::Heartbeat);
    }

    // ── File transfer messages ──────────────────────────────────────────

    #[test]
    fn test_file_transfer_start() {
        roundtrip_message(&Message::FileTransferStart {
            file_id: Uuid::new_v4(),
            file_name: "song.mp3".into(),
            size: 5_000_000,
            sha256: [0xAB; 32],
        });
    }

    #[test]
    fn test_file_chunk() {
        roundtrip_message(&Message::FileChunk {
            file_id: Uuid::new_v4(),
            offset: 0,
            data: vec![1, 2, 3, 4, 5],
        });
    }

    #[test]
    fn test_file_received() {
        roundtrip_message(&Message::FileReceived {
            file_id: Uuid::new_v4(),
            hash_ok: true,
        });
    }

    // ── Song request messages ───────────────────────────────────────────

    #[test]
    fn test_song_request() {
        roundtrip_message(&Message::SongRequest {
            file_name: "track.mp3".into(),
            file_size: 8_000_000,
        });
    }

    #[test]
    fn test_song_request_accepted() {
        roundtrip_message(&Message::SongRequestAccepted {
            request_id: Uuid::new_v4(),
        });
    }

    #[test]
    fn test_song_request_rejected() {
        roundtrip_message(&Message::SongRequestRejected {
            request_id: Uuid::new_v4(),
        });
    }

    #[test]
    fn test_song_upload_chunk() {
        roundtrip_message(&Message::SongUploadChunk {
            request_id: Uuid::new_v4(),
            offset: 64_000,
            data: vec![0xFF; 65_536],
        });
    }

    #[test]
    fn test_song_upload_complete() {
        roundtrip_message(&Message::SongUploadComplete {
            request_id: Uuid::new_v4(),
            sha256: [0xCD; 32],
        });
    }

    // ── Playback control messages ───────────────────────────────────────

    #[test]
    fn test_play_command() {
        roundtrip_message(&Message::PlayCommand {
            file_id: Uuid::new_v4(),
            position_samples: 44_100,
            target_time_ns: 1_000_000_000,
        });
    }

    #[test]
    fn test_pause_command() {
        roundtrip_message(&Message::PauseCommand {
            position_samples: 88_200,
        });
    }

    #[test]
    fn test_resume_command() {
        roundtrip_message(&Message::ResumeCommand {
            position_samples: 88_200,
            target_time_ns: 2_000_000_000,
        });
    }

    #[test]
    fn test_stop_command() {
        roundtrip_message(&Message::StopCommand);
    }

    #[test]
    fn test_seek_command() {
        roundtrip_message(&Message::SeekCommand {
            position_samples: 0,
            target_time_ns: 500_000_000,
        });
    }

    // ── Queue sync message ──────────────────────────────────────────────

    #[test]
    fn test_queue_update() {
        roundtrip_message(&Message::QueueUpdate {
            queue: vec![
                QueueItem {
                    id: Uuid::new_v4(),
                    file_name: "track1.mp3".into(),
                    duration_secs: 210.5,
                    added_by: "Alice".into(),
                    status: QueueItemStatus::Ready,
                },
                QueueItem {
                    id: Uuid::new_v4(),
                    file_name: "track2.mp3".into(),
                    duration_secs: 180.0,
                    added_by: "Bob".into(),
                    status: QueueItemStatus::Transferring,
                },
            ],
        });
    }

    // ── Clock sync messages ─────────────────────────────────────────────

    #[test]
    fn test_clock_ping() {
        roundtrip_clock(&ClockMessage::ClockPing {
            peer_id: 1,
            peer_send_time_ns: 123_456_789,
        });
    }

    #[test]
    fn test_clock_pong() {
        roundtrip_clock(&ClockMessage::ClockPong {
            peer_id: 1,
            peer_send_time_ns: 123_456_789,
            host_recv_time_ns: 123_457_000,
            host_send_time_ns: 123_457_100,
        });
    }

    // ── Framing round-trip tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_framed_heartbeat() {
        roundtrip_framed(&Message::Heartbeat).await;
    }

    #[tokio::test]
    async fn test_framed_join_accepted() {
        roundtrip_framed(&Message::JoinAccepted {
            peer_id: 42,
            session_state: SessionState {
                session_name: "Test Session".into(),
                host_name: "Host".into(),
                peers: vec![],
                queue: vec![],
                current_track: Some(CurrentTrack {
                    file_id: Uuid::new_v4(),
                    file_name: "now_playing.mp3".into(),
                    position_samples: 44_100,
                    sample_rate: 44_100,
                    is_playing: true,
                }),
            },
        })
        .await;
    }

    #[tokio::test]
    async fn test_framed_file_chunk() {
        roundtrip_framed(&Message::FileChunk {
            file_id: Uuid::new_v4(),
            offset: 0,
            data: vec![0xAA; 65_536],
        })
        .await;
    }

    #[tokio::test]
    async fn test_framed_play_command() {
        roundtrip_framed(&Message::PlayCommand {
            file_id: Uuid::new_v4(),
            position_samples: 0,
            target_time_ns: u64::MAX,
        })
        .await;
    }

    // ── Multiple messages on one stream ─────────────────────────────────

    #[tokio::test]
    async fn test_framed_multiple_messages() {
        let messages = vec![
            Message::Heartbeat,
            Message::JoinRequest { display_name: "Test".into() },
            Message::StopCommand,
            Message::LeaveSession { peer_id: 7 },
        ];

        let mut buf: Vec<u8> = Vec::new();
        for msg in &messages {
            write_message(&mut buf, msg).await.expect("write");
        }

        let mut cursor = Cursor::new(buf);
        for expected in &messages {
            let decoded = read_message(&mut cursor).await.expect("read");
            assert_eq!(*expected, decoded);
        }
    }

    // ── Edge case tests ─────────────────────────────────────────────────

    #[test]
    fn test_empty_string() {
        roundtrip_message(&Message::JoinRequest {
            display_name: String::new(),
        });
    }

    #[test]
    fn test_empty_data_vec() {
        roundtrip_message(&Message::FileChunk {
            file_id: Uuid::nil(),
            offset: 0,
            data: vec![],
        });
    }

    #[test]
    fn test_max_u64_values() {
        roundtrip_message(&Message::PlayCommand {
            file_id: Uuid::nil(),
            position_samples: u64::MAX,
            target_time_ns: u64::MAX,
        });
    }

    #[test]
    fn test_empty_queue_update() {
        roundtrip_message(&Message::QueueUpdate { queue: vec![] });
    }

    #[test]
    fn test_all_queue_item_statuses() {
        for status in [
            QueueItemStatus::Transferring,
            QueueItemStatus::Ready,
            QueueItemStatus::Playing,
            QueueItemStatus::Played,
        ] {
            let item = QueueItem {
                id: Uuid::new_v4(),
                file_name: "test.mp3".into(),
                duration_secs: 0.0,
                added_by: String::new(),
                status,
            };
            let bytes = bincode::serialize(&item).expect("serialize");
            let decoded: QueueItem = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(item, decoded);
        }
    }

    #[tokio::test]
    async fn test_framed_message_too_large() {
        // Craft a buffer with a length prefix that exceeds MAX_MESSAGE_SIZE.
        let fake_len: u32 = MAX_MESSAGE_SIZE + 1;
        let buf = fake_len.to_le_bytes().to_vec();
        let mut cursor = Cursor::new(buf);
        let result = read_message(&mut cursor).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, FrameError::MessageTooLarge(_)));
    }

    #[tokio::test]
    async fn test_framed_truncated_stream() {
        // Write a valid length prefix but not enough payload bytes.
        let mut buf: Vec<u8> = Vec::new();
        let len: u32 = 100;
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&[0u8; 10]); // only 10 bytes instead of 100

        let mut cursor = Cursor::new(buf);
        let result = read_message(&mut cursor).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FrameError::Io(_)));
    }
}
