use crate::network::mdns::MdnsBroadcaster;
use crate::network::messages::{Message, PeerInfo, SessionState};
use crate::network::tcp::{TcpEvent, TcpHost};
use crate::network::udp::UdpHost;
use crate::session::SessionEvent;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

// ── Constants ───────────────────────────────────────────────────────────────

/// Maximum number of peers allowed in a session (excluding the host).
const MAX_PEERS: usize = 5;

// ── Peer state ──────────────────────────────────────────────────────────────

/// State tracked by the host for each connected peer.
#[derive(Debug)]
pub struct PeerState {
    pub peer_id: u32,
    pub display_name: String,
    pub latency_ns: u64,
    /// File IDs this peer has confirmed receiving.
    pub files_ready: HashSet<uuid::Uuid>,
    pub connected_at: Instant,
    pub last_seen: Instant,
}

// ── HostSession ─────────────────────────────────────────────────────────────

/// The host-side session manager. Orchestrates TCP, UDP, and mDNS.
pub struct HostSession {
    pub session_name: String,
    pub host_display_name: String,
    pub peers: Arc<Mutex<HashMap<u32, PeerState>>>,
    pub tcp_host: Arc<TcpHost>,
    pub udp_host: Option<UdpHost>,
    pub mdns: Option<MdnsBroadcaster>,
    next_peer_id: Arc<AtomicU32>,
    event_tx: mpsc::Sender<SessionEvent>,
    tcp_port: u16,
    udp_port: u16,
    /// Handle to the TCP event processing loop.
    _event_loop_handle: Option<tokio::task::JoinHandle<()>>,
    /// Handle to the heartbeat sending task.
    _heartbeat_handle: Option<tokio::task::JoinHandle<()>>,
}

impl HostSession {
    /// Create and start a new host session.
    ///
    /// - Binds the TCP listener on `tcp_port`.
    /// - Starts the UDP clock-sync responder on `udp_port`.
    /// - Registers the session via mDNS (unless `skip_mdns` is true, for tests).
    /// - Spawns a background task that processes TCP events.
    ///
    /// The caller receives `SessionEvent`s on the returned receiver.
    pub async fn start(
        session_name: String,
        host_display_name: String,
        tcp_port: u16,
        udp_port: u16,
        skip_mdns: bool,
    ) -> Result<(Self, mpsc::Receiver<SessionEvent>), Box<dyn std::error::Error + Send + Sync>>
    {
        let (session_event_tx, session_event_rx) = mpsc::channel::<SessionEvent>(64);
        let (tcp_event_tx, tcp_event_rx) = mpsc::channel::<TcpEvent>(128);

        // ── TCP ──
        let tcp_host = Arc::new(TcpHost::start(tcp_port, tcp_event_tx).await?);

        // ── UDP ──
        let udp_host = match UdpHost::start(udp_port).await {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!("Failed to start UDP clock host: {e}");
                None
            }
        };

        // ── mDNS ──
        let mdns = if skip_mdns {
            None
        } else {
            match MdnsBroadcaster::register(
                &session_name,
                &host_display_name,
                tcp_port,
                0,
                MAX_PEERS as u8,
            ) {
                Ok(b) => Some(b),
                Err(e) => {
                    log::warn!("Failed to register mDNS: {e}");
                    None
                }
            }
        };

        let peers: Arc<Mutex<HashMap<u32, PeerState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Peer IDs start at 1. The TCP layer also assigns internal IDs starting
        // at 1, so they will match (we rely on this in the event loop).
        let next_peer_id = Arc::new(AtomicU32::new(1));

        let host = Self {
            session_name,
            host_display_name,
            peers: peers.clone(),
            tcp_host: tcp_host.clone(),
            udp_host,
            mdns,
            next_peer_id,
            event_tx: session_event_tx,
            tcp_port,
            udp_port,
            _event_loop_handle: None,
            _heartbeat_handle: None,
        };

        // Spawn the heartbeat task.
        let heartbeat_handle = tcp_host.start_heartbeat();

        // Spawn the TCP event processing loop.
        let event_loop_handle = Self::spawn_event_loop(
            tcp_event_rx,
            tcp_host.clone(),
            peers.clone(),
            host.event_tx.clone(),
            host.session_name.clone(),
            host.host_display_name.clone(),
            host.tcp_port,
            host.udp_port,
        );

        // We need to set the handles after construction since we used `self` fields.
        let mut host = host;
        host._event_loop_handle = Some(event_loop_handle);
        host._heartbeat_handle = Some(heartbeat_handle);

        log::info!(
            "Host session \"{}\" started on TCP {} / UDP {}",
            host.session_name,
            host.tcp_port,
            host.udp_port,
        );

        Ok((host, session_event_rx))
    }

    /// Background task that reads TcpEvents and translates them into session
    /// logic: join handling, leave handling, message dispatch.
    fn spawn_event_loop(
        mut tcp_event_rx: mpsc::Receiver<TcpEvent>,
        tcp_host: Arc<TcpHost>,
        peers: Arc<Mutex<HashMap<u32, PeerState>>>,
        event_tx: mpsc::Sender<SessionEvent>,
        session_name: String,
        host_display_name: String,
        tcp_port: u16,
        udp_port: u16,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(tcp_event) = tcp_event_rx.recv().await {
                match tcp_event {
                    TcpEvent::PeerConnected { peer_id, addr } => {
                        log::info!("TCP peer {peer_id} connected from {addr} — waiting for JoinRequest");
                    }

                    TcpEvent::MessageReceived { peer_id, message } => {
                        Self::handle_message(
                            peer_id,
                            message,
                            &tcp_host,
                            &peers,
                            &event_tx,
                            &session_name,
                            &host_display_name,
                            tcp_port,
                            udp_port,
                        )
                        .await;
                    }

                    TcpEvent::PeerDisconnected { peer_id, reason } => {
                        let was_present = {
                            let mut map = peers.lock();
                            map.remove(&peer_id).is_some()
                        };

                        if was_present {
                            log::info!("Peer {peer_id} disconnected: {reason}");

                            // Broadcast updated peer list.
                            let peer_list = Self::build_peer_list(&peers);
                            let queue = Vec::new(); // queue managed elsewhere
                            let state = Message::JoinAccepted {
                                peer_id: 0, // not used for broadcast
                                session_state: SessionState {
                                    session_name: session_name.clone(),
                                    host_name: host_display_name.clone(),
                                    peers: peer_list,
                                    queue,
                                    current_track: None,
                                },
                            };
                            tcp_host.broadcast(&state).await;

                            let _ = event_tx
                                .send(SessionEvent::PeerLeft { peer_id })
                                .await;
                        }
                    }
                }
            }
            log::debug!("Host event loop exiting");
        })
    }

    /// Process a single message from a peer.
    #[allow(clippy::too_many_arguments)]
    async fn handle_message(
        peer_id: u32,
        message: Message,
        tcp_host: &Arc<TcpHost>,
        peers: &Arc<Mutex<HashMap<u32, PeerState>>>,
        event_tx: &mpsc::Sender<SessionEvent>,
        session_name: &str,
        host_display_name: &str,
        _tcp_port: u16,
        _udp_port: u16,
    ) {
        match message {
            Message::JoinRequest { display_name } => {
                let peer_count = peers.lock().len();

                if peer_count >= MAX_PEERS {
                    let _ = tcp_host
                        .send_to_peer(
                            peer_id,
                            &Message::JoinRejected {
                                reason: "Session is full".into(),
                            },
                        )
                        .await;
                    // Disconnect after rejection.
                    tcp_host.disconnect_peer(peer_id);
                    return;
                }

                // Deduplicate display name.
                let final_name = Self::deduplicate_name(&display_name, peers);

                // Add to peer map.
                {
                    let mut map = peers.lock();
                    map.insert(
                        peer_id,
                        PeerState {
                            peer_id,
                            display_name: final_name.clone(),
                            latency_ns: 0,
                            files_ready: HashSet::new(),
                            connected_at: Instant::now(),
                            last_seen: Instant::now(),
                        },
                    );
                }

                // Update TCP layer's name.
                tcp_host.set_peer_name(peer_id, final_name.clone());

                // Build current session state.
                let peer_list = Self::build_peer_list(peers);
                let session_state = SessionState {
                    session_name: session_name.to_string(),
                    host_name: host_display_name.to_string(),
                    peers: peer_list.clone(),
                    queue: Vec::new(), // queue managed elsewhere
                    current_track: None,
                };

                // Send JoinAccepted to the new peer.
                let _ = tcp_host
                    .send_to_peer(
                        peer_id,
                        &Message::JoinAccepted {
                            peer_id,
                            session_state: session_state.clone(),
                        },
                    )
                    .await;

                // Broadcast updated state to all OTHER peers.
                let all_ids: Vec<u32> = peers.lock().keys().copied().collect();
                for pid in all_ids {
                    if pid != peer_id {
                        let _ = tcp_host
                            .send_to_peer(
                                pid,
                                &Message::JoinAccepted {
                                    peer_id: pid,
                                    session_state: session_state.clone(),
                                },
                            )
                            .await;
                    }
                }

                log::info!("Peer {peer_id} (\"{final_name}\") joined the session");

                let _ = event_tx
                    .send(SessionEvent::PeerJoined {
                        peer_id,
                        display_name: final_name,
                    })
                    .await;
            }

            Message::LeaveSession { .. } => {
                let was_present = {
                    let mut map = peers.lock();
                    map.remove(&peer_id).is_some()
                };

                if was_present {
                    log::info!("Peer {peer_id} left the session voluntarily");

                    tcp_host.disconnect_peer(peer_id);

                    // Broadcast updated peer list.
                    let peer_list = Self::build_peer_list(peers);
                    let state_msg = Message::JoinAccepted {
                        peer_id: 0,
                        session_state: SessionState {
                            session_name: session_name.to_string(),
                            host_name: host_display_name.to_string(),
                            peers: peer_list,
                            queue: Vec::new(),
                            current_track: None,
                        },
                    };
                    tcp_host.broadcast(&state_msg).await;

                    let _ = event_tx
                        .send(SessionEvent::PeerLeft { peer_id })
                        .await;
                }
            }

            Message::Heartbeat => {
                // Update last-seen.
                let mut map = peers.lock();
                if let Some(ps) = map.get_mut(&peer_id) {
                    ps.last_seen = Instant::now();
                }
            }

            Message::FileReceived { file_id, hash_ok } => {
                if hash_ok {
                    let mut map = peers.lock();
                    if let Some(ps) = map.get_mut(&peer_id) {
                        ps.files_ready.insert(file_id);
                    }
                }
                // Also forward to session event for the app to handle.
                let _ = event_tx
                    .send(SessionEvent::MessageReceived {
                        peer_id,
                        message: Message::FileReceived { file_id, hash_ok },
                    })
                    .await;
            }

            other => {
                // Forward any other message to the application layer.
                let _ = event_tx
                    .send(SessionEvent::MessageReceived {
                        peer_id,
                        message: other,
                    })
                    .await;
            }
        }
    }

    /// Build a `Vec<PeerInfo>` from the current peer map.
    fn build_peer_list(peers: &Arc<Mutex<HashMap<u32, PeerState>>>) -> Vec<PeerInfo> {
        let map = peers.lock();
        let mut list: Vec<PeerInfo> = map
            .values()
            .map(|ps| PeerInfo {
                peer_id: ps.peer_id,
                display_name: ps.display_name.clone(),
            })
            .collect();
        list.sort_by_key(|p| p.peer_id);
        list
    }

    /// If `name` already exists among the peers, append a number suffix.
    fn deduplicate_name(
        name: &str,
        peers: &Arc<Mutex<HashMap<u32, PeerState>>>,
    ) -> String {
        let map = peers.lock();
        let existing: HashSet<&str> = map.values().map(|ps| ps.display_name.as_str()).collect();

        if !existing.contains(name) {
            return name.to_string();
        }

        let mut n = 2u32;
        loop {
            let candidate = format!("{name} ({n})");
            if !existing.contains(candidate.as_str()) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Get the number of connected peers.
    pub fn peer_count(&self) -> usize {
        self.peers.lock().len()
    }

    /// Get the current peer list.
    pub fn peer_list(&self) -> Vec<PeerInfo> {
        Self::build_peer_list(&self.peers)
    }

    /// Send a message to a specific peer.
    pub async fn send_to_peer(&self, peer_id: u32, msg: &Message) -> bool {
        self.tcp_host.send_to_peer(peer_id, msg).await
    }

    /// Broadcast a message to all connected peers.
    pub async fn broadcast(&self, msg: &Message) {
        self.tcp_host.broadcast(msg).await;
    }

    /// Shut down the session: notify all peers, unregister mDNS, close everything.
    pub async fn shutdown(mut self) {
        log::info!("Shutting down host session \"{}\"", self.session_name);

        // Send SessionEnd to all peers.
        self.tcp_host.broadcast(&Message::SessionEnd).await;

        // Brief pause so the messages can be flushed.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Abort the event loop and heartbeat tasks.
        if let Some(h) = self._event_loop_handle.take() {
            h.abort();
        }
        if let Some(h) = self._heartbeat_handle.take() {
            h.abort();
        }

        // Shut down TCP.
        // We need to get a mutable reference — use Arc::try_unwrap or get_mut.
        // Since we own `self`, no one else should hold the Arc, but the event
        // loop may still have a clone. We just abort it above, so give a tick.
        tokio::task::yield_now().await;

        if let Some(tcp) = Arc::get_mut(&mut self.tcp_host) {
            tcp.shutdown();
        }

        // Shut down UDP.
        if let Some(udp) = self.udp_host.take() {
            udp.shutdown();
        }

        // Unregister mDNS.
        if let Some(mdns) = self.mdns.take() {
            let _ = mdns.unregister();
        }

        self.peers.lock().clear();
        log::info!("Host session shut down");
    }
}
