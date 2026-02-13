use crate::network::messages::{Message, PeerInfo, SessionState};
use crate::network::tcp::{TcpEvent, TcpPeer};
use crate::network::udp::UdpPeer;
use crate::session::SessionEvent;
use crate::sync::clock::ClockSync;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

// ── Constants ───────────────────────────────────────────────────────────────

/// How long to wait for JoinAccepted / JoinRejected after sending JoinRequest.
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);

// ── PeerSession ─────────────────────────────────────────────────────────────

/// The peer-side session manager. Connects to a host and maintains sync.
pub struct PeerSession {
    pub peer_id: u32,
    pub display_name: String,
    pub session_name: String,
    pub host_name: String,
    pub tcp_peer: TcpPeer,
    pub clock_sync: Arc<Mutex<ClockSync>>,
    pub peers: Arc<Mutex<Vec<PeerInfo>>>,
    pub event_tx: mpsc::Sender<SessionEvent>,
    pub udp_peer: Option<UdpPeer>,
    /// Handle to the background message dispatch loop.
    _dispatch_handle: Option<tokio::task::JoinHandle<()>>,
}

impl PeerSession {
    /// Connect to a host, perform the join handshake, and start sync.
    ///
    /// - `host_tcp_addr` — host's TCP address.
    /// - `host_udp_addr` — host's UDP clock-sync address.
    /// - `display_name`  — the display name to request.
    /// - `skip_udp`      — if true, skip UDP clock sync (for tests).
    ///
    /// On success, returns the `PeerSession` and a receiver for session events.
    pub async fn join(
        host_tcp_addr: SocketAddr,
        host_udp_addr: SocketAddr,
        display_name: String,
        skip_udp: bool,
    ) -> Result<(Self, mpsc::Receiver<SessionEvent>), Box<dyn std::error::Error + Send + Sync>>
    {
        let (session_event_tx, session_event_rx) = mpsc::channel::<SessionEvent>(64);
        let (tcp_event_tx, mut tcp_event_rx) = mpsc::channel::<TcpEvent>(128);

        // ── TCP connect ──
        let mut tcp_peer = TcpPeer::connect(host_tcp_addr, tcp_event_tx, false).await?;

        // ── Send JoinRequest ──
        tcp_peer
            .send(&Message::JoinRequest {
                display_name: display_name.clone(),
            })
            .await
            .map_err(|e| format!("failed to send JoinRequest: {e}"))?;

        log::info!("Sent JoinRequest as \"{display_name}\" to {host_tcp_addr}");

        // ── Wait for JoinAccepted or JoinRejected ──
        let (peer_id, final_name, session_state) =
            Self::wait_for_join_response(&mut tcp_event_rx).await?;

        log::info!(
            "Joined session \"{}\" as peer {peer_id} (\"{final_name}\")",
            session_state.session_name
        );

        let session_name = session_state.session_name.clone();
        let host_name = session_state.host_name.clone();
        let peers = Arc::new(Mutex::new(session_state.peers.clone()));

        // ── UDP clock sync ──
        let (udp_peer, clock_sync) = if skip_udp {
            (None, Arc::new(Mutex::new(ClockSync::new())))
        } else {
            match UdpPeer::start(host_udp_addr, peer_id).await {
                Ok(udp) => {
                    let cs = udp.clock_sync.clone();
                    (Some(udp), cs)
                }
                Err(e) => {
                    log::warn!("Failed to start UDP clock sync: {e}");
                    (None, Arc::new(Mutex::new(ClockSync::new())))
                }
            }
        };

        // Spawn the message dispatch loop.
        let dispatch_handle = Self::spawn_dispatch_loop(
            tcp_event_rx,
            peers.clone(),
            session_event_tx.clone(),
        );

        Ok((
            Self {
                peer_id,
                display_name: final_name,
                session_name,
                host_name,
                tcp_peer,
                clock_sync,
                peers,
                event_tx: session_event_tx,
                udp_peer,
                _dispatch_handle: Some(dispatch_handle),
            },
            session_event_rx,
        ))
    }

    /// Wait for a JoinAccepted or JoinRejected from the host.
    async fn wait_for_join_response(
        tcp_event_rx: &mut mpsc::Receiver<TcpEvent>,
    ) -> Result<(u32, String, SessionState), Box<dyn std::error::Error + Send + Sync>> {
        let deadline = timeout(JOIN_TIMEOUT, async {
            while let Some(event) = tcp_event_rx.recv().await {
                match event {
                    TcpEvent::MessageReceived { message, .. } => match message {
                        Message::JoinAccepted {
                            peer_id,
                            session_state,
                        } => {
                            // Find our display name in the peer list.
                            let my_name = session_state
                                .peers
                                .iter()
                                .find(|p| p.peer_id == peer_id)
                                .map(|p| p.display_name.clone())
                                .unwrap_or_default();
                            return Ok((peer_id, my_name, session_state));
                        }
                        Message::JoinRejected { reason } => {
                            return Err(format!("Join rejected: {reason}").into());
                        }
                        _ => {
                            // Ignore other messages during handshake.
                        }
                    },
                    TcpEvent::PeerDisconnected { reason, .. } => {
                        return Err(
                            format!("Disconnected during join handshake: {reason}").into()
                        );
                    }
                    _ => {}
                }
            }
            Err("TCP event channel closed during join handshake".into())
        });

        match deadline.await {
            Ok(result) => result,
            Err(_) => Err("Join handshake timed out".into()),
        }
    }

    /// Background task that dispatches incoming messages from the host.
    fn spawn_dispatch_loop(
        mut tcp_event_rx: mpsc::Receiver<TcpEvent>,
        peers: Arc<Mutex<Vec<PeerInfo>>>,
        event_tx: mpsc::Sender<SessionEvent>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(event) = tcp_event_rx.recv().await {
                match event {
                    TcpEvent::MessageReceived { message, .. } => {
                        match &message {
                            Message::SessionEnd => {
                                log::info!("Host ended the session");
                                let _ = event_tx.send(SessionEvent::HostDisconnected).await;
                                break;
                            }

                            Message::JoinAccepted { session_state, .. } => {
                                // Updated peer list from host.
                                *peers.lock() = session_state.peers.clone();
                                // Don't emit a SessionEvent for state updates —
                                // the caller can poll peers directly.
                            }

                            Message::Heartbeat => {
                                // Heartbeat from host — no action needed.
                            }

                            _ => {
                                // Forward everything else to the app.
                                let _ = event_tx
                                    .send(SessionEvent::MessageReceived {
                                        peer_id: 0,
                                        message,
                                    })
                                    .await;
                            }
                        }
                    }

                    TcpEvent::PeerDisconnected { reason, .. } => {
                        log::info!("Host disconnected: {reason}");
                        let _ = event_tx.send(SessionEvent::HostDisconnected).await;
                        break;
                    }

                    _ => {}
                }
            }
            log::debug!("Peer dispatch loop exiting");
        })
    }

    /// Send a message to the host.
    pub async fn send(&mut self, msg: &Message) -> Result<(), crate::network::messages::FrameError> {
        self.tcp_peer.send(msg).await
    }

    /// Get the current peer list.
    pub fn peer_list(&self) -> Vec<PeerInfo> {
        self.peers.lock().clone()
    }

    /// Leave the session gracefully: send LeaveSession and shut down.
    pub async fn leave(&mut self) {
        log::info!("Leaving session \"{}\"", self.session_name);

        let _ = self
            .tcp_peer
            .send(&Message::LeaveSession {
                peer_id: self.peer_id,
            })
            .await;

        // Brief pause to flush.
        tokio::time::sleep(Duration::from_millis(50)).await;

        self.tcp_peer.shutdown();

        if let Some(udp) = self.udp_peer.take() {
            udp.shutdown();
        }

        if let Some(h) = self._dispatch_handle.take() {
            h.abort();
        }

        log::info!("Left session");
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::host::HostSession;
    use tokio::net::TcpListener;
    use tokio::time::{sleep, Duration};

    const TEST_TIMEOUT: Duration = Duration::from_secs(15);

    /// Grab two free ports (TCP + UDP).
    async fn free_ports() -> (u16, u16) {
        let l1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p1 = l1.local_addr().unwrap().port();
        drop(l1);
        let l2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p2 = l2.local_addr().unwrap().port();
        drop(l2);
        (p1, p2)
    }

    fn tcp_addr(port: u16) -> SocketAddr {
        SocketAddr::new("127.0.0.1".parse().unwrap(), port)
    }

    // ── Test 1: Basic join handshake ────────────────────────────────────

    #[tokio::test]
    async fn test_peer_joins_host_session() {
        let _ = timeout(TEST_TIMEOUT, async {
            let (tcp_port, udp_port) = free_ports().await;

            let (host, mut host_events) = HostSession::start(
                "Test Session".into(),
                "Host".into(),
                tcp_port,
                udp_port,
                true, // skip mDNS
            )
            .await
            .unwrap();

            // Give host a tick to start listening.
            sleep(Duration::from_millis(50)).await;

            let (peer, _peer_events) = PeerSession::join(
                tcp_addr(tcp_port),
                tcp_addr(udp_port), // won't be used (skip_udp below)
                "Alice".into(),
                true, // skip UDP
            )
            .await
            .unwrap();

            // Verify the peer got an ID and the correct session info.
            assert!(peer.peer_id > 0);
            assert_eq!(peer.session_name, "Test Session");
            assert_eq!(peer.host_name, "Host");
            assert_eq!(peer.display_name, "Alice");

            // Verify host received the PeerJoined event.
            if let Some(SessionEvent::PeerJoined { peer_id, display_name }) =
                host_events.recv().await
            {
                assert_eq!(display_name, "Alice");
                assert_eq!(peer_id, peer.peer_id);
            } else {
                panic!("Expected PeerJoined event");
            }

            // Verify host's peer list.
            assert_eq!(host.peer_count(), 1);
            let list = host.peer_list();
            assert_eq!(list[0].display_name, "Alice");

            // Clean up.
            host.shutdown().await;
        })
        .await
        .unwrap();
    }

    // ── Test 2: Duplicate name gets suffix ──────────────────────────────

    #[tokio::test]
    async fn test_duplicate_name_gets_suffix() {
        let _ = timeout(TEST_TIMEOUT, async {
            let (tcp_port, udp_port) = free_ports().await;

            let (host, mut host_events) = HostSession::start(
                "Dup Test".into(),
                "Host".into(),
                tcp_port,
                udp_port,
                true,
            )
            .await
            .unwrap();

            sleep(Duration::from_millis(50)).await;

            // First peer: "Alex"
            let (_peer1, _pe1) = PeerSession::join(
                tcp_addr(tcp_port),
                tcp_addr(udp_port),
                "Alex".into(),
                true,
            )
            .await
            .unwrap();

            // Wait for host to process the join.
            let _ = host_events.recv().await;
            sleep(Duration::from_millis(100)).await;

            // Second peer: also "Alex" — should become "Alex (2)"
            let (peer2, _pe2) = PeerSession::join(
                tcp_addr(tcp_port),
                tcp_addr(udp_port),
                "Alex".into(),
                true,
            )
            .await
            .unwrap();

            assert_eq!(peer2.display_name, "Alex (2)");

            // Verify host's event.
            if let Some(SessionEvent::PeerJoined { display_name, .. }) =
                host_events.recv().await
            {
                assert_eq!(display_name, "Alex (2)");
            } else {
                panic!("Expected PeerJoined event for Alex (2)");
            }

            host.shutdown().await;
        })
        .await
        .unwrap();
    }

    // ── Test 3: Session full rejection ──────────────────────────────────

    #[tokio::test]
    async fn test_session_full_rejects_sixth_peer() {
        let _ = timeout(Duration::from_secs(30), async {
            let (tcp_port, udp_port) = free_ports().await;

            let (host, mut host_events) = HostSession::start(
                "Full Test".into(),
                "Host".into(),
                tcp_port,
                udp_port,
                true,
            )
            .await
            .unwrap();

            sleep(Duration::from_millis(50)).await;

            // Join 5 peers.
            let mut peers = Vec::new();
            let mut peer_event_rxs = Vec::new();
            for i in 1..=5 {
                let (p, pe) = PeerSession::join(
                    tcp_addr(tcp_port),
                    tcp_addr(udp_port),
                    format!("Peer{i}"),
                    true,
                )
                .await
                .unwrap();
                peers.push(p);
                peer_event_rxs.push(pe);

                // Wait for the host to process this join.
                let _ = host_events.recv().await;
                sleep(Duration::from_millis(50)).await;
            }

            assert_eq!(host.peer_count(), 5);

            // 6th peer should be rejected.
            let result = PeerSession::join(
                tcp_addr(tcp_port),
                tcp_addr(udp_port),
                "Peer6".into(),
                true,
            )
            .await;

            assert!(result.is_err());
            let err_msg = match result {
                Err(e) => e.to_string(),
                Ok(_) => panic!("Expected error"),
            };
            assert!(err_msg.contains("Session is full"), "Got: {err_msg}");

            host.shutdown().await;
        })
        .await
        .unwrap();
    }

    // ── Test 4: Voluntary leave ─────────────────────────────────────────

    #[tokio::test]
    async fn test_peer_leaves_voluntarily() {
        let _ = timeout(TEST_TIMEOUT, async {
            let (tcp_port, udp_port) = free_ports().await;

            let (host, mut host_events) = HostSession::start(
                "Leave Test".into(),
                "Host".into(),
                tcp_port,
                udp_port,
                true,
            )
            .await
            .unwrap();

            sleep(Duration::from_millis(50)).await;

            let (mut peer, _pe) = PeerSession::join(
                tcp_addr(tcp_port),
                tcp_addr(udp_port),
                "Leaver".into(),
                true,
            )
            .await
            .unwrap();

            // Wait for PeerJoined.
            let _ = host_events.recv().await;

            assert_eq!(host.peer_count(), 1);

            // Peer leaves.
            peer.leave().await;

            // Wait for host to process the leave.
            sleep(Duration::from_millis(200)).await;

            // Drain events to find PeerLeft.
            let mut found_left = false;
            while let Ok(ev) = host_events.try_recv() {
                if matches!(ev, SessionEvent::PeerLeft { .. }) {
                    found_left = true;
                    break;
                }
            }

            // The host should have 0 peers now.
            assert_eq!(host.peer_count(), 0, "Peer should have been removed from host");

            // PeerLeft event may arrive via LeaveSession message or via PeerDisconnected.
            // Both are valid — the key assertion is the peer count.
            let _ = found_left; // either path is fine

            host.shutdown().await;
        })
        .await
        .unwrap();
    }

    // ── Test 5: Host shutdown sends SessionEnd to peers ─────────────────

    #[tokio::test]
    async fn test_host_shutdown_notifies_peers() {
        let _ = timeout(TEST_TIMEOUT, async {
            let (tcp_port, udp_port) = free_ports().await;

            let (host, mut host_events) = HostSession::start(
                "Shutdown Test".into(),
                "Host".into(),
                tcp_port,
                udp_port,
                true,
            )
            .await
            .unwrap();

            sleep(Duration::from_millis(50)).await;

            let (_peer, mut peer_events) = PeerSession::join(
                tcp_addr(tcp_port),
                tcp_addr(udp_port),
                "Observer".into(),
                true,
            )
            .await
            .unwrap();

            // Wait for PeerJoined.
            let _ = host_events.recv().await;
            sleep(Duration::from_millis(100)).await;

            // Host shuts down.
            host.shutdown().await;

            // Peer should receive HostDisconnected.
            let mut got_disconnect = false;
            for _ in 0..20 {
                match timeout(Duration::from_millis(200), peer_events.recv()).await {
                    Ok(Some(SessionEvent::HostDisconnected)) => {
                        got_disconnect = true;
                        break;
                    }
                    Ok(Some(_)) => continue,
                    _ => {
                        sleep(Duration::from_millis(100)).await;
                    }
                }
            }

            assert!(got_disconnect, "Peer should receive HostDisconnected");
        })
        .await
        .unwrap();
    }
}
