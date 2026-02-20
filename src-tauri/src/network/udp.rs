use crate::network::messages::ClockMessage;
use crate::sync::clock::{ClockSync, HostClockTracker};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio::time::{self, Duration};

// ── Constants ───────────────────────────────────────────────────────────────

/// Default UDP port for clock synchronization.
pub const DEFAULT_CLOCK_PORT: u16 = 17402;

/// Interval between clock pings sent by the peer.
const PING_INTERVAL: Duration = Duration::from_millis(200);

/// Maximum datagram size for clock messages.
const MAX_DGRAM_SIZE: usize = 256;

/// How often the host broadcasts session info (listeners list) to all peers.
const SESSION_BROADCAST_INTERVAL: Duration = Duration::from_secs(2);

/// Maximum datagram size for session update messages (names can be long).
const MAX_SESSION_DGRAM_SIZE: usize = 2048;

// ── Monotonic clock helper ──────────────────────────────────────────────────

/// A reference instant used to convert `Instant` to nanosecond timestamps.
static BASE_INSTANT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Get the base instant (lazily initialized on first call).
pub fn base_instant() -> Instant {
    *BASE_INSTANT.get_or_init(Instant::now)
}

/// Current monotonic time in nanoseconds since the base instant.
pub fn now_ns() -> u64 {
    base_instant().elapsed().as_nanos() as u64
}

// ── UdpHost ─────────────────────────────────────────────────────────────────

/// Host-side UDP listener for clock synchronization and session info broadcast.
pub struct UdpHost {
    pub tracker: Arc<Mutex<HostClockTracker>>,
    /// Known peer UDP addresses (peer_id → last-seen SocketAddr).
    pub peer_addrs: Arc<Mutex<HashMap<u32, SocketAddr>>>,
    /// Shared session info for periodic broadcast: (host_name, listeners list).
    /// Updated externally by the session layer.
    pub session_info: Arc<Mutex<(String, Vec<(u32, String)>)>>,
    shutdown_tx: broadcast::Sender<()>,
    _listener_handle: tokio::task::JoinHandle<()>,
    _broadcast_handle: tokio::task::JoinHandle<()>,
}

impl UdpHost {
    /// Bind on `port` and start responding to `ClockPing` messages.
    /// Also starts a periodic session-info broadcast every 2 seconds.
    pub async fn start(port: u16) -> Result<Self, std::io::Error> {
        let socket = Arc::new(UdpSocket::bind(("0.0.0.0", port)).await?);
        let local_addr = socket.local_addr()?;
        log::info!("UDP clock host listening on {local_addr}");

        let tracker = Arc::new(Mutex::new(HostClockTracker::new()));
        let peer_addrs: Arc<Mutex<HashMap<u32, SocketAddr>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let session_info: Arc<Mutex<(String, Vec<(u32, String)>)>> =
            Arc::new(Mutex::new((String::new(), Vec::new())));
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(1);

        // ── Listener task ──
        let sock = socket.clone();
        let trk = tracker.clone();
        let addrs = peer_addrs.clone();

        let listener_handle = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DGRAM_SIZE];
            loop {
                tokio::select! {
                    result = sock.recv_from(&mut buf) => {
                        match result {
                            Ok((len, src_addr)) => {
                                Self::handle_datagram(&sock, &trk, &addrs, &buf[..len], src_addr).await;
                            }
                            Err(e) => {
                                log::error!("UDP recv error: {e}");
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        log::debug!("UDP host: shutdown signal");
                        break;
                    }
                }
            }
        });

        // ── Session broadcast task ──
        let sock_bc = socket.clone();
        let addrs_bc = peer_addrs.clone();
        let info_bc = session_info.clone();
        let mut sd_rx_bc = shutdown_tx.subscribe();

        let broadcast_handle = tokio::spawn(async move {
            let mut interval = time::interval(SESSION_BROADCAST_INTERVAL);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let (host_name, listeners) = info_bc.lock().clone();
                        if host_name.is_empty() {
                            continue; // not configured yet
                        }
                        let msg = ClockMessage::SessionUpdate { host_name, listeners };
                        let bytes = match bincode::serialize(&msg) {
                            Ok(b) => b,
                            Err(e) => {
                                log::debug!("UDP: failed to serialize SessionUpdate: {e}");
                                continue;
                            }
                        };
                        let addrs_snapshot: Vec<SocketAddr> = addrs_bc.lock().values().copied().collect();
                        for addr in addrs_snapshot {
                            if let Err(e) = sock_bc.send_to(&bytes, addr).await {
                                log::debug!("UDP: failed to send SessionUpdate to {addr}: {e}");
                            }
                        }
                    }
                    _ = sd_rx_bc.recv() => break,
                }
            }
        });

        Ok(Self {
            tracker,
            peer_addrs,
            session_info,
            shutdown_tx,
            _listener_handle: listener_handle,
            _broadcast_handle: broadcast_handle,
        })
    }

    async fn handle_datagram(
        socket: &UdpSocket,
        tracker: &Arc<Mutex<HostClockTracker>>,
        peer_addrs: &Arc<Mutex<HashMap<u32, SocketAddr>>>,
        data: &[u8],
        src_addr: SocketAddr,
    ) {
        let host_recv_time_ns = now_ns();

        let msg: ClockMessage = match bincode::deserialize(data) {
            Ok(m) => m,
            Err(e) => {
                log::debug!("UDP: invalid datagram from {src_addr}: {e}");
                return;
            }
        };

        match msg {
            ClockMessage::ClockPing {
                peer_id,
                peer_send_time_ns,
                peer_measured_latency_ns,
            } => {
                let host_send_time_ns = now_ns();

                // Remember peer address for session broadcasts.
                peer_addrs.lock().insert(peer_id, src_addr);

                // Store the peer's own RTT-based latency estimate.
                tracker.lock().record_ping(
                    peer_id,
                    peer_measured_latency_ns,
                );

                let pong = ClockMessage::ClockPong {
                    peer_id,
                    peer_send_time_ns,
                    host_recv_time_ns,
                    host_send_time_ns,
                };

                let pong_bytes = match bincode::serialize(&pong) {
                    Ok(b) => b,
                    Err(e) => {
                        log::error!("UDP: failed to serialize ClockPong: {e}");
                        return;
                    }
                };

                if let Err(e) = socket.send_to(&pong_bytes, src_addr).await {
                    log::debug!("UDP: failed to send pong to {src_addr}: {e}");
                }
            }
            ClockMessage::ClockPong { .. } => {
                log::debug!("UDP host: ignoring unexpected ClockPong from {src_addr}");
            }
            ClockMessage::SessionUpdate { .. } => {
                // Host ignores its own message type.
            }
        }
    }

    /// Shut down the listener.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }
}

impl Drop for UdpHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── UdpPeer ─────────────────────────────────────────────────────────────────

/// Peer-side UDP client that pings the host and maintains clock sync state.
pub struct UdpPeer {
    pub clock_sync: Arc<Mutex<ClockSync>>,
    /// Latest session info received from the host: (host_name, listeners).
    pub session_info: Arc<Mutex<Option<(String, Vec<(u32, String)>)>>>,
    shutdown_tx: broadcast::Sender<()>,
    _ping_handle: tokio::task::JoinHandle<()>,
    _recv_handle: tokio::task::JoinHandle<()>,
}

impl UdpPeer {
    /// Start clock synchronization with a host at `host_addr`.
    /// `peer_id` is this peer's ID (assigned during the TCP join).
    pub async fn start(
        host_addr: SocketAddr,
        peer_id: u32,
    ) -> Result<Self, std::io::Error> {
        let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        let local_addr = socket.local_addr()?;
        log::info!("UDP clock peer bound on {local_addr}, targeting host {host_addr}");

        let clock_sync = Arc::new(Mutex::new(ClockSync::new()));
        let session_info: Arc<Mutex<Option<(String, Vec<(u32, String)>)>>> =
            Arc::new(Mutex::new(None));
        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        // Ping task: send ClockPing every PING_INTERVAL.
        let sock_ping = socket.clone();
        let cs_ping = clock_sync.clone();
        let mut sd_rx_ping = shutdown_tx.subscribe();
        let ping_handle = tokio::spawn(async move {
            let mut interval = time::interval(PING_INTERVAL);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let ping = ClockMessage::ClockPing {
                            peer_id,
                            peer_send_time_ns: now_ns(),
                            peer_measured_latency_ns: cs_ping.lock().current_latency_ns,
                        };
                        if let Ok(bytes) = bincode::serialize(&ping) {
                            if let Err(e) = sock_ping.send_to(&bytes, host_addr).await {
                                log::debug!("UDP: failed to send ping: {e}");
                            }
                        }
                    }
                    _ = sd_rx_ping.recv() => break,
                }
            }
        });

        // Receive task: listen for ClockPong / SessionUpdate.
        let sock_recv = socket.clone();
        let cs = clock_sync.clone();
        let si = session_info.clone();
        let mut sd_rx_recv = shutdown_tx.subscribe();
        let recv_handle = tokio::spawn(async move {
            let mut buf = [0u8; MAX_SESSION_DGRAM_SIZE];
            loop {
                tokio::select! {
                    result = sock_recv.recv_from(&mut buf) => {
                        match result {
                            Ok((len, _src)) => {
                                match bincode::deserialize::<ClockMessage>(&buf[..len]) {
                                    Ok(ClockMessage::ClockPong {
                                        peer_send_time_ns,
                                        host_recv_time_ns,
                                        host_send_time_ns,
                                        ..
                                    }) => {
                                        let peer_recv_time_ns = now_ns();
                                        cs.lock().record_pong(
                                            peer_send_time_ns,
                                            host_recv_time_ns,
                                            host_send_time_ns,
                                            peer_recv_time_ns,
                                        );
                                    }
                                    Ok(ClockMessage::SessionUpdate { host_name, listeners }) => {
                                        *si.lock() = Some((host_name, listeners));
                                    }
                                    Ok(_) => {} // ignore pings
                                    Err(_) => {} // ignore malformed
                                }
                            }
                            Err(e) => {
                                log::debug!("UDP peer recv error: {e}");
                            }
                        }
                    }
                    _ = sd_rx_recv.recv() => break,
                }
            }
        });

        Ok(Self {
            clock_sync,
            session_info,
            shutdown_tx,
            _ping_handle: ping_handle,
            _recv_handle: recv_handle,
        })
    }

    /// Shut down the ping/receive tasks.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }
}

impl Drop for UdpPeer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket as TokioUdpSocket;
    use tokio::time::{sleep, timeout, Duration};

    /// Get a random available UDP port.
    async fn free_udp_port() -> u16 {
        let sock = TokioUdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn test_clock_sync_converges_on_loopback() {
        let port = free_udp_port().await;

        // Start host.
        let host = UdpHost::start(port).await.unwrap();

        // Start peer targeting the host.
        let host_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let peer = UdpPeer::start(host_addr, 1).await.unwrap();

        // Let it run for 3 seconds (~15 pings).
        sleep(Duration::from_secs(3)).await;

        let cs = peer.clock_sync.lock();

        // On loopback, offset should be very close to zero (< 5ms).
        let offset_ms = cs.current_offset_ns.unsigned_abs() as f64 / 1_000_000.0;
        assert!(
            offset_ms < 5.0,
            "offset should be near zero on loopback, got {offset_ms}ms"
        );

        // RTT on loopback should be very low (< 1ms = 1_000_000 ns).
        assert!(
            cs.current_latency_ns < 1_000_000,
            "latency should be < 1ms on loopback, got {}ns",
            cs.current_latency_ns
        );

        // Should have accumulated multiple measurements.
        assert!(
            cs.measurement_count() >= 5,
            "expected at least 5 measurements, got {}",
            cs.measurement_count()
        );

        drop(cs);

        // Host should have latency tracking for peer 1.
        let lat = host.tracker.lock().get_latency(1).cloned();
        assert!(lat.is_some(), "host should track peer 1 latency");

        // Cleanup.
        peer.shutdown();
        host.shutdown();
    }

    #[tokio::test]
    async fn test_host_responds_to_manual_ping() {
        let port = free_udp_port().await;
        let _host = UdpHost::start(port).await.unwrap();

        // Manually send a ClockPing.
        let sock = TokioUdpSocket::bind("127.0.0.1:0").await.unwrap();
        let host_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let ping = ClockMessage::ClockPing {
            peer_id: 42,
            peer_send_time_ns: 123_456,
            peer_measured_latency_ns: 0,
        };
        let bytes = bincode::serialize(&ping).unwrap();
        sock.send_to(&bytes, host_addr).await.unwrap();

        // Wait for pong.
        let mut buf = [0u8; MAX_DGRAM_SIZE];
        let (len, _) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .expect("timeout waiting for pong")
            .unwrap();

        let pong: ClockMessage = bincode::deserialize(&buf[..len]).unwrap();
        match pong {
            ClockMessage::ClockPong {
                peer_id,
                peer_send_time_ns,
                host_recv_time_ns,
                host_send_time_ns,
            } => {
                assert_eq!(peer_id, 42);
                assert_eq!(peer_send_time_ns, 123_456);
                assert!(host_recv_time_ns > 0);
                assert!(host_send_time_ns >= host_recv_time_ns);
            }
            other => panic!("Expected ClockPong, got {other:?}"),
        }
    }
}
