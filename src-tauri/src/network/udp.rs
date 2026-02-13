use crate::network::messages::ClockMessage;
use crate::sync::clock::{ClockSync, HostClockTracker};
use parking_lot::Mutex;
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

// ── Monotonic clock helper ──────────────────────────────────────────────────

/// A reference instant used to convert `Instant` to nanosecond timestamps.
static BASE_INSTANT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Get the base instant (lazily initialized on first call).
fn base_instant() -> Instant {
    *BASE_INSTANT.get_or_init(Instant::now)
}

/// Current monotonic time in nanoseconds since the base instant.
pub fn now_ns() -> u64 {
    base_instant().elapsed().as_nanos() as u64
}

// ── UdpHost ─────────────────────────────────────────────────────────────────

/// Host-side UDP listener for clock synchronization.
pub struct UdpHost {
    pub tracker: Arc<Mutex<HostClockTracker>>,
    shutdown_tx: broadcast::Sender<()>,
    _listener_handle: tokio::task::JoinHandle<()>,
}

impl UdpHost {
    /// Bind on `port` and start responding to `ClockPing` messages.
    pub async fn start(port: u16) -> Result<Self, std::io::Error> {
        let socket = Arc::new(UdpSocket::bind(("0.0.0.0", port)).await?);
        let local_addr = socket.local_addr()?;
        log::info!("UDP clock host listening on {local_addr}");

        let tracker = Arc::new(Mutex::new(HostClockTracker::new()));
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(1);

        let sock = socket.clone();
        let trk = tracker.clone();

        let listener_handle = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DGRAM_SIZE];
            loop {
                tokio::select! {
                    result = sock.recv_from(&mut buf) => {
                        match result {
                            Ok((len, src_addr)) => {
                                Self::handle_datagram(&sock, &trk, &buf[..len], src_addr).await;
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

        Ok(Self {
            tracker,
            shutdown_tx,
            _listener_handle: listener_handle,
        })
    }

    async fn handle_datagram(
        socket: &UdpSocket,
        tracker: &Arc<Mutex<HostClockTracker>>,
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
            } => {
                let host_send_time_ns = now_ns();

                // Track peer latency on host side.
                tracker.lock().record_ping(
                    peer_id,
                    peer_send_time_ns,
                    host_recv_time_ns,
                    host_send_time_ns,
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
        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        // Ping task: send ClockPing every PING_INTERVAL.
        let sock_ping = socket.clone();
        let mut sd_rx_ping = shutdown_tx.subscribe();
        let ping_handle = tokio::spawn(async move {
            let mut interval = time::interval(PING_INTERVAL);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let ping = ClockMessage::ClockPing {
                            peer_id,
                            peer_send_time_ns: now_ns(),
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

        // Receive task: listen for ClockPong and update ClockSync.
        let sock_recv = socket.clone();
        let cs = clock_sync.clone();
        let mut sd_rx_recv = shutdown_tx.subscribe();
        let recv_handle = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DGRAM_SIZE];
            loop {
                tokio::select! {
                    result = sock_recv.recv_from(&mut buf) => {
                        match result {
                            Ok((len, _src)) => {
                                let peer_recv_time_ns = now_ns();
                                if let Ok(ClockMessage::ClockPong {
                                    peer_send_time_ns,
                                    host_recv_time_ns,
                                    host_send_time_ns,
                                    ..
                                }) = bincode::deserialize(&buf[..len])
                                {
                                    cs.lock().record_pong(
                                        peer_send_time_ns,
                                        host_recv_time_ns,
                                        host_send_time_ns,
                                        peer_recv_time_ns,
                                    );
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
