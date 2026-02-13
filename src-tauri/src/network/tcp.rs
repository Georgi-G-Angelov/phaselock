use crate::network::messages::{read_message, write_message, FrameError, Message};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{self, Duration, Instant};

// ── Constants ───────────────────────────────────────────────────────────────

/// Default TCP port for PhaseLock sessions.
pub const DEFAULT_PORT: u16 = 17401;

/// Interval between heartbeat messages.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

/// If no message is received within this duration, the connection is dead.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);

// ── Event types ─────────────────────────────────────────────────────────────

/// Events produced by the TCP layer and sent to the session manager.
#[derive(Debug)]
pub enum TcpEvent {
    /// A new peer connected (before JoinRequest is received).
    PeerConnected { peer_id: u32, addr: SocketAddr },
    /// A message was received from a peer.
    MessageReceived { peer_id: u32, message: Message },
    /// A peer disconnected (EOF, error, or heartbeat timeout).
    PeerDisconnected { peer_id: u32, reason: String },
}

// ── TcpHost ─────────────────────────────────────────────────────────────────

/// A peer's write half plus metadata, stored in the host's connection map.
struct PeerConnection {
    pub display_name: String,
    pub writer: OwnedWriteHalf,
    #[allow(dead_code)]
    pub connected_at: Instant,
}

/// Host-side TCP server: accepts connections, reads/writes framed messages.
pub struct TcpHost {
    connections: Arc<Mutex<HashMap<u32, PeerConnection>>>,
    next_peer_id: Arc<Mutex<u32>>,
    /// Handle to the accept-loop task so we can abort on shutdown.
    accept_handle: Option<tokio::task::JoinHandle<()>>,
    /// Sender used to cancel per-peer reader/heartbeat tasks.
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
}

impl TcpHost {
    /// Bind and start accepting connections. Received events flow into `event_tx`.
    pub async fn start(
        port: u16,
        event_tx: mpsc::Sender<TcpEvent>,
    ) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        let local_addr = listener.local_addr()?;
        log::info!("TCP host listening on {local_addr}");

        let connections: Arc<Mutex<HashMap<u32, PeerConnection>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let next_peer_id = Arc::new(Mutex::new(1u32));
        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

        let conns = connections.clone();
        let next_id = next_peer_id.clone();
        let sd_tx = shutdown_tx.clone();

        let accept_handle = tokio::spawn(async move {
            loop {
                let (stream, addr) = match listener.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("TCP accept error: {e}");
                        continue;
                    }
                };

                let peer_id = {
                    let mut id = next_id.lock();
                    let pid = *id;
                    *id += 1;
                    pid
                };

                log::info!("Peer {peer_id} connected from {addr}");

                let (reader, writer) = stream.into_split();

                {
                    let mut map = conns.lock();
                    map.insert(
                        peer_id,
                        PeerConnection {
                            display_name: String::new(),
                            writer,
                            connected_at: Instant::now(),
                        },
                    );
                }

                let _ = event_tx
                    .send(TcpEvent::PeerConnected { peer_id, addr })
                    .await;

                // Spawn reader + heartbeat-timeout task for this peer.
                let ev_tx = event_tx.clone();
                let conns2 = conns.clone();
                let mut sd_rx = sd_tx.subscribe();

                tokio::spawn(async move {
                    let mut reader = reader;
                    let mut last_received = Instant::now();

                    loop {
                        tokio::select! {
                            result = read_message(&mut reader) => {
                                match result {
                                    Ok(msg) => {
                                        last_received = Instant::now();
                                        log::debug!("Peer {peer_id}: received {msg:?}");
                                        let _ = ev_tx.send(TcpEvent::MessageReceived {
                                            peer_id,
                                            message: msg,
                                        }).await;
                                    }
                                    Err(FrameError::Io(ref e))
                                        if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                                    {
                                        log::info!("Peer {peer_id}: connection closed (EOF)");
                                        break;
                                    }
                                    Err(e) => {
                                        log::info!("Peer {peer_id}: read error: {e}");
                                        break;
                                    }
                                }
                            }
                            _ = tokio::time::sleep_until(last_received + HEARTBEAT_TIMEOUT) => {
                                log::info!("Peer {peer_id}: heartbeat timeout");
                                let _ = ev_tx.send(TcpEvent::PeerDisconnected {
                                    peer_id,
                                    reason: "heartbeat timeout".into(),
                                }).await;
                                conns2.lock().remove(&peer_id);
                                return;
                            }
                            _ = sd_rx.recv() => {
                                log::debug!("Peer {peer_id}: shutdown signal");
                                break;
                            }
                        }
                    }

                    // Clean up on exit.
                    let _ = ev_tx
                        .send(TcpEvent::PeerDisconnected {
                            peer_id,
                            reason: "disconnected".into(),
                        })
                        .await;
                    conns2.lock().remove(&peer_id);
                });
            }
        });

        Ok(Self {
            connections,
            next_peer_id,
            accept_handle: Some(accept_handle),
            shutdown_tx,
        })
    }

    /// Send a message to a specific peer. Returns false if the peer is gone.
    pub async fn send_to_peer(&self, peer_id: u32, msg: &Message) -> bool {
        // Take the writer out briefly — we need &mut and can't hold the mutex
        // across an await, so we remove, write, then re-insert.
        let mut writer = {
            let mut map = self.connections.lock();
            match map.remove(&peer_id) {
                Some(conn) => (conn.display_name.clone(), conn.writer, conn.connected_at),
                None => return false,
            }
        };

        let success = write_message(&mut writer.1, msg).await.is_ok();

        if success {
            log::debug!("Sent to peer {peer_id}: {msg:?}");
            let mut map = self.connections.lock();
            map.insert(
                peer_id,
                PeerConnection {
                    display_name: writer.0,
                    writer: writer.1,
                    connected_at: writer.2,
                },
            );
        } else {
            log::info!("Failed to send to peer {peer_id}, dropping connection");
        }

        success
    }

    /// Broadcast a message to all connected peers.
    pub async fn broadcast(&self, msg: &Message) {
        let peer_ids: Vec<u32> = {
            let map = self.connections.lock();
            map.keys().copied().collect()
        };
        for pid in peer_ids {
            self.send_to_peer(pid, msg).await;
        }
    }

    /// Disconnect and remove a peer.
    pub fn disconnect_peer(&self, peer_id: u32) {
        let removed = self.connections.lock().remove(&peer_id);
        if let Some(mut conn) = removed {
            // Shutdown the write half so the remote reader gets EOF.
            let _ = conn.writer.shutdown();
            log::info!("Peer {peer_id} disconnected by host");
        }
    }

    /// Update a peer's display name (called when JoinRequest is processed).
    pub fn set_peer_name(&self, peer_id: u32, name: String) {
        if let Some(conn) = self.connections.lock().get_mut(&peer_id) {
            conn.display_name = name;
        }
    }

    /// Returns the list of currently connected peer IDs.
    pub fn connected_peer_ids(&self) -> Vec<u32> {
        self.connections.lock().keys().copied().collect()
    }

    /// Start a background task that sends heartbeats to all peers at a fixed interval.
    pub fn start_heartbeat(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let host = Arc::clone(self);
        let mut sd_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut interval = time::interval(HEARTBEAT_INTERVAL);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        host.broadcast(&Message::Heartbeat).await;
                    }
                    _ = sd_rx.recv() => break,
                }
            }
        })
    }

    /// Shut down the host: stop accepting, signal all reader tasks, close all connections.
    pub fn shutdown(&mut self) {
        if let Some(handle) = self.accept_handle.take() {
            handle.abort();
        }
        let _ = self.shutdown_tx.send(());
        let mut map = self.connections.lock();
        for (pid, mut conn) in map.drain() {
            let _ = conn.writer.shutdown();
            log::info!("Peer {pid} disconnected (host shutdown)");
        }
    }
}

impl Drop for TcpHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── TcpPeer ─────────────────────────────────────────────────────────────────

/// Peer-side TCP client: connects to a host, reads/writes framed messages.
pub struct TcpPeer {
    writer: Option<OwnedWriteHalf>,
    _reader_handle: tokio::task::JoinHandle<()>,
    _heartbeat_handle: Option<tokio::task::JoinHandle<()>>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
}

impl TcpPeer {
    /// Connect to a host and start the reader loop. Events flow into `event_tx`.
    /// If `send_heartbeats` is true, a heartbeat task is also spawned.
    pub async fn connect(
        addr: SocketAddr,
        event_tx: mpsc::Sender<TcpEvent>,
        send_heartbeats: bool,
    ) -> Result<Self, std::io::Error> {
        let stream = TcpStream::connect(addr).await?;
        log::info!("Connected to host at {addr}");

        let (reader, writer) = stream.into_split();
        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        let mut sd_rx = shutdown_tx.subscribe();

        let ev_tx = event_tx.clone();
        let reader_handle = tokio::spawn(async move {
            let mut reader = reader;
            let mut last_received = Instant::now();

            loop {
                tokio::select! {
                    result = read_message(&mut reader) => {
                        match result {
                            Ok(msg) => {
                                last_received = Instant::now();
                                log::debug!("Host: received {msg:?}");
                                let _ = ev_tx.send(TcpEvent::MessageReceived {
                                    peer_id: 0, // 0 = host
                                    message: msg,
                                }).await;
                            }
                            Err(FrameError::Io(ref e))
                                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                            {
                                log::info!("Host connection closed (EOF)");
                                break;
                            }
                            Err(e) => {
                                log::info!("Host read error: {e}");
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep_until(last_received + HEARTBEAT_TIMEOUT) => {
                        log::info!("Host heartbeat timeout");
                        let _ = ev_tx.send(TcpEvent::PeerDisconnected {
                            peer_id: 0,
                            reason: "host heartbeat timeout".into(),
                        }).await;
                        return;
                    }
                    _ = sd_rx.recv() => {
                        log::debug!("Peer reader: shutdown signal");
                        break;
                    }
                }
            }

            let _ = ev_tx
                .send(TcpEvent::PeerDisconnected {
                    peer_id: 0,
                    reason: "disconnected".into(),
                })
                .await;
        });

        let heartbeat_handle = if send_heartbeats {
            // Heartbeat sending from peer side is handled externally
            // by the session layer calling send(Heartbeat) on an interval.
            None
        } else {
            None
        };

        Ok(Self {
            writer: Some(writer),
            _reader_handle: reader_handle,
            _heartbeat_handle: heartbeat_handle,
            shutdown_tx,
        })
    }

    /// Send a message to the host.
    pub async fn send(&mut self, msg: &Message) -> Result<(), FrameError> {
        if let Some(ref mut writer) = self.writer {
            log::debug!("Sending to host: {msg:?}");
            write_message(writer, msg).await
        } else {
            Err(FrameError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not connected",
            )))
        }
    }

    /// Shut down the connection.
    pub fn shutdown(&mut self) {
        let _ = self.shutdown_tx.send(());
        if let Some(mut w) = self.writer.take() {
            // Use a blocking approach since Drop can't be async.
            tokio::spawn(async move {
                let _ = w.shutdown().await;
            });
        }
    }
}

impl Drop for TcpPeer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::messages::{PeerInfo, SessionState};
    use tokio::time::timeout;

    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// Get a random available port by binding to port 0.
    async fn free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    // ── Basic send / receive ────────────────────────────────────────────

    #[tokio::test]
    async fn test_peer_sends_join_request_host_receives() {
        let port = free_port().await;
        let (host_tx, mut host_rx) = mpsc::channel(32);

        let _host = TcpHost::start(port, host_tx).await.unwrap();

        // Connect a peer.
        let (peer_tx, _peer_rx) = mpsc::channel(32);
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut peer = TcpPeer::connect(addr, peer_tx, false).await.unwrap();

        // Wait for PeerConnected event on host side.
        let ev = timeout(TEST_TIMEOUT, host_rx.recv()).await.unwrap().unwrap();
        assert!(matches!(ev, TcpEvent::PeerConnected { .. }));

        // Peer sends a JoinRequest.
        peer.send(&Message::JoinRequest {
            display_name: "Alice".into(),
        })
        .await
        .unwrap();

        // Host receives the JoinRequest.
        let ev = timeout(TEST_TIMEOUT, host_rx.recv()).await.unwrap().unwrap();
        match ev {
            TcpEvent::MessageReceived { peer_id, message } => {
                assert_eq!(peer_id, 1);
                assert!(matches!(message, Message::JoinRequest { display_name } if display_name == "Alice"));
            }
            other => panic!("Expected MessageReceived, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_host_sends_join_accepted_peer_receives() {
        let port = free_port().await;
        let (host_tx, mut host_rx) = mpsc::channel(32);

        let host = TcpHost::start(port, host_tx).await.unwrap();

        let (peer_tx, mut peer_rx) = mpsc::channel(32);
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let _peer = TcpPeer::connect(addr, peer_tx, false).await.unwrap();

        // Wait for PeerConnected.
        let ev = timeout(TEST_TIMEOUT, host_rx.recv()).await.unwrap().unwrap();
        let peer_id = match ev {
            TcpEvent::PeerConnected { peer_id, .. } => peer_id,
            other => panic!("Expected PeerConnected, got {other:?}"),
        };

        // Host sends JoinAccepted to the peer.
        let msg = Message::JoinAccepted {
            peer_id,
            session_state: SessionState {
                session_name: "Test".into(),
                host_name: "Host".into(),
                peers: vec![PeerInfo {
                    peer_id,
                    display_name: "Alice".into(),
                }],
                queue: vec![],
                current_track: None,
            },
        };
        assert!(host.send_to_peer(peer_id, &msg).await);

        // Peer receives it.
        let ev = timeout(TEST_TIMEOUT, peer_rx.recv()).await.unwrap().unwrap();
        match ev {
            TcpEvent::MessageReceived { message, .. } => {
                assert!(matches!(message, Message::JoinAccepted { .. }));
            }
            other => panic!("Expected MessageReceived, got {other:?}"),
        }
    }

    // ── Broadcast ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_broadcast_to_multiple_peers() {
        let port = free_port().await;
        let (host_tx, mut host_rx) = mpsc::channel(32);

        let host = TcpHost::start(port, host_tx).await.unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // Connect peer 1.
        let (p1_tx, mut p1_rx) = mpsc::channel(32);
        let _peer1 = TcpPeer::connect(addr, p1_tx, false).await.unwrap();
        let _ = timeout(TEST_TIMEOUT, host_rx.recv()).await; // PeerConnected

        // Connect peer 2.
        let (p2_tx, mut p2_rx) = mpsc::channel(32);
        let _peer2 = TcpPeer::connect(addr, p2_tx, false).await.unwrap();
        let _ = timeout(TEST_TIMEOUT, host_rx.recv()).await; // PeerConnected

        // Broadcast a message.
        host.broadcast(&Message::StopCommand).await;

        // Both peers should receive it.
        let ev1 = timeout(TEST_TIMEOUT, p1_rx.recv()).await.unwrap().unwrap();
        assert!(matches!(
            ev1,
            TcpEvent::MessageReceived {
                message: Message::StopCommand,
                ..
            }
        ));

        let ev2 = timeout(TEST_TIMEOUT, p2_rx.recv()).await.unwrap().unwrap();
        assert!(matches!(
            ev2,
            TcpEvent::MessageReceived {
                message: Message::StopCommand,
                ..
            }
        ));
    }

    // ── Heartbeat timeout ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_heartbeat_timeout_disconnects_peer() {
        let port = free_port().await;
        let (host_tx, mut host_rx) = mpsc::channel(32);

        let _host = TcpHost::start(port, host_tx).await.unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // Connect a peer but don't send anything — it will be disconnected
        // by the host's reader timeout.
        let (peer_tx, _peer_rx) = mpsc::channel(32);
        let _peer = TcpPeer::connect(addr, peer_tx, false).await.unwrap();

        // Wait for PeerConnected.
        let _ = timeout(TEST_TIMEOUT, host_rx.recv()).await;

        // Wait for PeerDisconnected (should arrive within ~5 seconds).
        let ev = timeout(Duration::from_secs(8), host_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ev {
            TcpEvent::PeerDisconnected { reason, .. } => {
                assert!(reason.contains("heartbeat timeout"));
            }
            other => panic!("Expected PeerDisconnected, got {other:?}"),
        }
    }

    // ── Clean disconnect ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_clean_disconnect_on_leave() {
        let port = free_port().await;
        let (host_tx, mut host_rx) = mpsc::channel(32);

        let _host = TcpHost::start(port, host_tx).await.unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let (peer_tx, _peer_rx) = mpsc::channel(32);
        let mut peer = TcpPeer::connect(addr, peer_tx, false).await.unwrap();

        // Wait for PeerConnected.
        let ev = timeout(TEST_TIMEOUT, host_rx.recv()).await.unwrap().unwrap();
        let peer_id = match ev {
            TcpEvent::PeerConnected { peer_id, .. } => peer_id,
            other => panic!("Expected PeerConnected, got {other:?}"),
        };

        // Peer sends LeaveSession and drops the connection.
        peer.send(&Message::LeaveSession { peer_id }).await.unwrap();
        peer.shutdown();

        // Host should receive the LeaveSession message, then a disconnect event.
        let ev = timeout(TEST_TIMEOUT, host_rx.recv()).await.unwrap().unwrap();
        assert!(matches!(
            ev,
            TcpEvent::MessageReceived {
                message: Message::LeaveSession { .. },
                ..
            }
        ));

        let ev = timeout(TEST_TIMEOUT, host_rx.recv()).await.unwrap().unwrap();
        assert!(matches!(ev, TcpEvent::PeerDisconnected { .. }));
    }
}
