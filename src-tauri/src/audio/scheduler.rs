use crate::audio::playback::AudioOutput;
use crate::network::messages::Message;
use crate::network::tcp::TcpHost;
use crate::network::udp::{base_instant, now_ns};
use crate::session::host::PeerState;
use crate::sync::clock::{ClockSync, HostClockTracker};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

// ── Constants ───────────────────────────────────────────────────────────────

/// Safety margin added on top of the maximum peer latency.
/// Accounts for jitter, processing time, and scheduling granularity.
const SAFETY_MARGIN: Duration = Duration::from_millis(50);

// ── SchedulerError ──────────────────────────────────────────────────────────

/// Errors that can occur during playback scheduling.
#[derive(Debug)]
pub enum SchedulerError {
    NoPeers,
    SendFailed(String),
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPeers => write!(f, "No peers in session"),
            Self::SendFailed(e) => write!(f, "Failed to send command: {e}"),
        }
    }
}

impl std::error::Error for SchedulerError {}

// ── Staggered dispatch plan (testable) ──────────────────────────────────────

/// A per-peer send plan computed from latency data.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerDispatchPlan {
    pub peer_id: u32,
    /// How long to wait before sending the command to this peer.
    pub send_delay: Duration,
}

/// Compute the target time and per-peer dispatch plan.
///
/// Returns `(target_time_ns, plans)` where `target_time_ns` is the absolute
/// monotonic timestamp (in the host's clock) at which all peers should start.
///
/// The algorithm:
/// 1. Find `max_latency` = max of all peer one-way latencies.
/// 2. `target_time_ns = now_ns + max_latency + SAFETY_MARGIN`.
/// 3. For each peer: `send_delay = max_latency - peer_latency`.
///    The highest-latency peer gets the command immediately (delay = 0),
///    lower-latency peers get it later so the command arrives at the same time.
pub fn compute_dispatch_plan(
    peer_latencies: &HashMap<u32, u64>,
    current_time_ns: u64,
) -> (u64, Vec<PeerDispatchPlan>) {
    if peer_latencies.is_empty() {
        let target = current_time_ns + SAFETY_MARGIN.as_nanos() as u64;
        return (target, Vec::new());
    }

    let max_latency_ns = peer_latencies.values().copied().max().unwrap_or(0);
    let target_time_ns = current_time_ns + max_latency_ns + SAFETY_MARGIN.as_nanos() as u64;

    let mut plans: Vec<PeerDispatchPlan> = peer_latencies
        .iter()
        .map(|(&peer_id, &latency_ns)| {
            let delay_ns = max_latency_ns.saturating_sub(latency_ns);
            PeerDispatchPlan {
                peer_id,
                send_delay: Duration::from_nanos(delay_ns),
            }
        })
        .collect();

    // Sort by peer_id for deterministic ordering.
    plans.sort_by_key(|p| p.peer_id);

    (target_time_ns, plans)
}

// ── Peer-side command handler ───────────────────────────────────────────────

/// Convert a host-clock `target_time_ns` to a local `Instant` on the peer,
/// using the peer's `ClockSync` offset and the shared `base_instant`.
pub fn host_target_to_local_instant(
    clock_sync: &ClockSync,
    target_time_ns: u64,
) -> Instant {
    let local_ns = clock_sync.host_to_local(target_time_ns);
    let bi = base_instant();
    bi + Duration::from_nanos(local_ns)
}

/// Handle a `PlayCommand` on the peer side.
pub fn peer_handle_play(
    audio: &AudioOutput,
    clock_sync: &ClockSync,
    position_samples: u64,
    target_time_ns: u64,
) {
    let target = host_target_to_local_instant(clock_sync, target_time_ns);
    audio.seek(position_samples);
    audio.play_at(target);
    log::info!(
        "Peer: scheduled play at +{:.1}ms, pos={}",
        target.saturating_duration_since(Instant::now()).as_secs_f64() * 1000.0,
        position_samples
    );
}

/// Handle a `ResumeCommand` on the peer side.
pub fn peer_handle_resume(
    audio: &AudioOutput,
    clock_sync: &ClockSync,
    position_samples: u64,
    target_time_ns: u64,
) {
    let target = host_target_to_local_instant(clock_sync, target_time_ns);
    audio.seek(position_samples);
    audio.resume_at(target);
    log::info!(
        "Peer: scheduled resume at +{:.1}ms, pos={}",
        target.saturating_duration_since(Instant::now()).as_secs_f64() * 1000.0,
        position_samples
    );
}

/// Handle a `SeekCommand` on the peer side.
pub fn peer_handle_seek(
    audio: &AudioOutput,
    clock_sync: &ClockSync,
    position_samples: u64,
    target_time_ns: u64,
) {
    let target = host_target_to_local_instant(clock_sync, target_time_ns);
    audio.seek(position_samples);
    audio.resume_at(target);
    log::info!(
        "Peer: scheduled seek at +{:.1}ms, pos={}",
        target.saturating_duration_since(Instant::now()).as_secs_f64() * 1000.0,
        position_samples
    );
}

/// Handle a `PauseCommand` on the peer side (immediate).
pub fn peer_handle_pause(audio: &AudioOutput, position_samples: u64) {
    audio.seek(position_samples);
    audio.pause();
    log::info!("Peer: paused at pos={}", position_samples);
}

/// Handle a `StopCommand` on the peer side (immediate).
pub fn peer_handle_stop(audio: &AudioOutput) {
    audio.stop();
    log::info!("Peer: stopped");
}

// ── PlaybackScheduler (host side) ───────────────────────────────────────────

/// Host-side playback scheduler. Dispatches time-critical playback commands
/// to peers with staggered timing based on per-peer latencies.
pub struct PlaybackScheduler {
    pub host_clock_tracker: Arc<Mutex<HostClockTracker>>,
    pub tcp_host: Arc<TcpHost>,
    pub audio_output: Option<Arc<AudioOutput>>,
    pub sample_rate: u32,
}

impl PlaybackScheduler {
    /// Create a new scheduler.
    pub fn new(
        host_clock_tracker: Arc<Mutex<HostClockTracker>>,
        tcp_host: Arc<TcpHost>,
        audio_output: Option<Arc<AudioOutput>>,
        sample_rate: u32,
    ) -> Self {
        Self {
            host_clock_tracker,
            tcp_host,
            audio_output,
            sample_rate,
        }
    }

    /// Schedule synchronized play across all peers + local audio.
    pub async fn play(
        &self,
        file_id: Uuid,
        position_samples: u64,
        peers: &HashMap<u32, PeerState>,
    ) -> Result<(), SchedulerError> {
        let (target_time_ns, plans) = self.build_plan(peers);

        let msg = Message::PlayCommand {
            file_id,
            position_samples,
            target_time_ns,
            sample_rate: self.sample_rate,
        };

        self.dispatch_staggered(plans, msg).await;

        // Schedule local playback.
        if let Some(ref audio) = self.audio_output {
            let bi = base_instant();
            let target_instant = bi + Duration::from_nanos(target_time_ns);
            audio.seek(position_samples);
            audio.play_at(target_instant);
        }

        log::info!(
            "Host: scheduled play file={} pos={} target=+{}ms",
            file_id,
            position_samples,
            (target_time_ns.saturating_sub(now_ns())) as f64 / 1_000_000.0
        );

        Ok(())
    }

    /// Schedule synchronized resume across all peers.
    pub async fn resume(
        &self,
        position_samples: u64,
        peers: &HashMap<u32, PeerState>,
    ) -> Result<(), SchedulerError> {
        let (target_time_ns, plans) = self.build_plan(peers);

        let msg = Message::ResumeCommand {
            position_samples,
            target_time_ns,
            sample_rate: self.sample_rate,
        };

        self.dispatch_staggered(plans, msg).await;

        if let Some(ref audio) = self.audio_output {
            let bi = base_instant();
            let target_instant = bi + Duration::from_nanos(target_time_ns);
            audio.seek(position_samples);
            audio.resume_at(target_instant);
        }

        log::info!("Host: scheduled resume pos={}", position_samples);
        Ok(())
    }

    /// Schedule synchronized seek across all peers.
    pub async fn seek(
        &self,
        position_samples: u64,
        peers: &HashMap<u32, PeerState>,
    ) -> Result<(), SchedulerError> {
        let (target_time_ns, plans) = self.build_plan(peers);

        let msg = Message::SeekCommand {
            position_samples,
            target_time_ns,
            sample_rate: self.sample_rate,
        };

        self.dispatch_staggered(plans, msg).await;

        if let Some(ref audio) = self.audio_output {
            let bi = base_instant();
            let target_instant = bi + Duration::from_nanos(target_time_ns);
            audio.seek(position_samples);
            audio.resume_at(target_instant);
        }

        log::info!("Host: scheduled seek pos={}", position_samples);
        Ok(())
    }

    /// Pause playback (broadcast immediately, no staggering).
    pub async fn pause(&self, position_samples: u64) -> Result<(), SchedulerError> {
        let msg = Message::PauseCommand { position_samples, sample_rate: self.sample_rate };
        self.tcp_host.broadcast(&msg).await;

        if let Some(ref audio) = self.audio_output {
            audio.seek(position_samples);
            audio.pause();
        }

        log::info!("Host: paused at pos={}", position_samples);
        Ok(())
    }

    /// Stop playback (broadcast immediately, no staggering).
    pub async fn stop(&self) -> Result<(), SchedulerError> {
        let msg = Message::StopCommand;
        self.tcp_host.broadcast(&msg).await;

        if let Some(ref audio) = self.audio_output {
            audio.stop();
        }

        log::info!("Host: stopped playback");
        Ok(())
    }

    /// Build a dispatch plan from the current peer latencies.
    fn build_plan(&self, _peers: &HashMap<u32, PeerState>) -> (u64, Vec<PeerDispatchPlan>) {
        let latencies = self.host_clock_tracker.lock().get_all_latencies();
        compute_dispatch_plan(&latencies, now_ns())
    }

    /// Send a message to each peer with the appropriate staggered delay.
    async fn dispatch_staggered(&self, plans: Vec<PeerDispatchPlan>, msg: Message) {
        let tcp = self.tcp_host.clone();

        for plan in plans {
            let tcp = tcp.clone();
            let msg = msg.clone();
            tokio::spawn(async move {
                if !plan.send_delay.is_zero() {
                    tokio::time::sleep(plan.send_delay).await;
                }
                let ok = tcp.send_to_peer(plan.peer_id, &msg).await;
                if !ok {
                    log::warn!(
                        "Failed to send staggered command to peer {}",
                        plan.peer_id
                    );
                }
            });
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit tests for compute_dispatch_plan ─────────────────────────────

    #[test]
    fn test_dispatch_plan_three_peers() {
        // 3 peers with latencies 5ms, 20ms, 50ms.
        let mut latencies = HashMap::new();
        latencies.insert(1, 5_000_000); // 5 ms in ns
        latencies.insert(2, 20_000_000); // 20 ms
        latencies.insert(3, 50_000_000); // 50 ms

        let now = 1_000_000_000u64; // 1 second
        let (target, plans) = compute_dispatch_plan(&latencies, now);

        // target = now + max_latency(50ms) + safety(50ms) = now + 100ms
        let expected_target = now + 50_000_000 + 50_000_000;
        assert_eq!(target, expected_target);

        // Plans sorted by peer_id.
        assert_eq!(plans.len(), 3);

        // peer 1: delay = 50ms - 5ms = 45ms
        assert_eq!(plans[0].peer_id, 1);
        assert_eq!(plans[0].send_delay, Duration::from_millis(45));

        // peer 2: delay = 50ms - 20ms = 30ms
        assert_eq!(plans[1].peer_id, 2);
        assert_eq!(plans[1].send_delay, Duration::from_millis(30));

        // peer 3: delay = 50ms - 50ms = 0ms (sent immediately)
        assert_eq!(plans[2].peer_id, 3);
        assert_eq!(plans[2].send_delay, Duration::from_millis(0));
    }

    #[test]
    fn test_dispatch_plan_single_peer() {
        let mut latencies = HashMap::new();
        latencies.insert(42, 10_000_000); // 10 ms

        let now = 500_000_000;
        let (target, plans) = compute_dispatch_plan(&latencies, now);

        // target = now + 10ms + 50ms = now + 60ms
        assert_eq!(target, now + 10_000_000 + 50_000_000);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].peer_id, 42);
        assert_eq!(plans[0].send_delay, Duration::ZERO);
    }

    #[test]
    fn test_dispatch_plan_equal_latencies() {
        let mut latencies = HashMap::new();
        latencies.insert(1, 15_000_000);
        latencies.insert(2, 15_000_000);

        let now = 0;
        let (target, plans) = compute_dispatch_plan(&latencies, now);

        assert_eq!(target, 15_000_000 + 50_000_000);
        // Both peers get delay = 0 (same latency).
        for plan in &plans {
            assert_eq!(plan.send_delay, Duration::ZERO);
        }
    }

    #[test]
    fn test_dispatch_plan_no_peers() {
        let latencies = HashMap::new();
        let now = 1_000_000_000;
        let (target, plans) = compute_dispatch_plan(&latencies, now);

        assert_eq!(target, now + 50_000_000);
        assert!(plans.is_empty());
    }

    #[test]
    fn test_dispatch_plan_zero_latency() {
        let mut latencies = HashMap::new();
        latencies.insert(1, 0);
        latencies.insert(2, 0);

        let now = 1_000_000_000;
        let (target, plans) = compute_dispatch_plan(&latencies, now);

        // max_latency = 0, so target = now + safety only.
        assert_eq!(target, now + 50_000_000);
        for plan in &plans {
            assert_eq!(plan.send_delay, Duration::ZERO);
        }
    }

    // ── Peer-side host_to_local conversion tests ────────────────────────

    #[test]
    fn test_host_to_local_zero_offset() {
        let cs = ClockSync::new(); // offset = 0
        let host_ns = 1_000_000_000u64;
        let local_ns = cs.host_to_local(host_ns);
        assert_eq!(local_ns, host_ns);
    }

    #[test]
    fn test_host_to_local_positive_offset() {
        let mut cs = ClockSync::new();
        // Simulate an offset: peer clock is 5ms ahead of host clock.
        // offset_ns = +5_000_000 (peer − host = +5ms)
        cs.current_offset_ns = 5_000_000;

        let host_ns = 100_000_000u64; // 100ms in host clock
        let local_ns = cs.host_to_local(host_ns);
        assert_eq!(local_ns, 105_000_000); // 100ms + 5ms = 105ms in local
    }

    #[test]
    fn test_host_to_local_negative_offset() {
        let mut cs = ClockSync::new();
        // Peer clock is 3ms behind host clock.
        cs.current_offset_ns = -3_000_000;

        let host_ns = 100_000_000u64;
        let local_ns = cs.host_to_local(host_ns);
        assert_eq!(local_ns, 97_000_000); // 100ms - 3ms = 97ms
    }

    #[test]
    fn test_host_target_to_local_instant() {
        let cs = ClockSync::new(); // offset = 0
        let bi = base_instant();
        let target_ns = 500_000_000u64; // 500ms

        let instant = host_target_to_local_instant(&cs, target_ns);
        let expected = bi + Duration::from_nanos(target_ns);
        // Should be very close (within microseconds of computation time).
        let diff = if instant > expected {
            instant.duration_since(expected)
        } else {
            expected.duration_since(instant)
        };
        assert!(
            diff < Duration::from_millis(1),
            "Instant diff too large: {:?}",
            diff
        );
    }

    // ── Integration-style test: scheduler with loopback ─────────────────

    /// Get a random available port by binding to port 0.
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn test_scheduler_dispatches_play_to_peers() {
        use crate::network::messages::{read_message, Message};
        use crate::network::tcp::TcpHost;
        use tokio::net::TcpStream;
        use tokio::sync::mpsc;

        let port = free_port().await;
        let (event_tx, _event_rx) = mpsc::channel(64);
        let host = TcpHost::start(port, event_tx).await.unwrap();
        let host = Arc::new(host);

        // Connect two "peers" (raw TCP connections).
        let addr = format!("127.0.0.1:{port}");
        let mut stream1 = TcpStream::connect(&addr).await.unwrap();
        let mut stream2 = TcpStream::connect(&addr).await.unwrap();

        // Give the host time to accept.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Get the peer IDs the host assigned.
        let peer_ids = host.connected_peer_ids();
        assert_eq!(peer_ids.len(), 2);

        // Set up the clock tracker with known latencies.
        let tracker = Arc::new(Mutex::new(HostClockTracker::new()));
        {
            let mut t = tracker.lock();
            // Manually insert latencies for each peer.
            // peer_ids[0] = 10ms, peer_ids[1] = 30ms
            t.record_ping(peer_ids[0], 10_000_000); // 10ms one-way
            t.record_ping(peer_ids[1], 30_000_000); // 30ms one-way
        }

        let scheduler = PlaybackScheduler::new(tracker, host.clone(), None, 44100);

        // Build a fake PeerState map.
        let mut peers = HashMap::new();
        for &pid in &peer_ids {
            peers.insert(
                pid,
                PeerState {
                    peer_id: pid,
                    display_name: format!("Peer{pid}"),
                    latency_ns: 0,
                    files_ready: Default::default(),
                    connected_at: Instant::now(),
                    last_seen: Instant::now(),
                },
            );
        }

        let file_id = Uuid::new_v4();
        scheduler.play(file_id, 0, &peers).await.unwrap();

        // Both peers should receive a PlayCommand.
        // Give staggered sends time to complete.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let msg1 = read_message(&mut stream1).await.unwrap();
        let msg2 = read_message(&mut stream2).await.unwrap();

        // Extract target_time_ns from both.
        let target1 = match &msg1 {
            Message::PlayCommand { target_time_ns, .. } => *target_time_ns,
            other => panic!("Expected PlayCommand, got {:?}", other),
        };

        let target2 = match &msg2 {
            Message::PlayCommand { target_time_ns, .. } => *target_time_ns,
            other => panic!("Expected PlayCommand, got {:?}", other),
        };

        // Both peers should receive the same target_time_ns.
        assert_eq!(target1, target2, "Both peers should target the same time");
    }

    #[tokio::test]
    async fn test_scheduler_broadcasts_pause_immediately() {
        use crate::network::messages::{read_message, Message};
        use crate::network::tcp::TcpHost;
        use tokio::net::TcpStream;
        use tokio::sync::mpsc;

        let port = free_port().await;
        let (event_tx, _event_rx) = mpsc::channel(64);
        let host = TcpHost::start(port, event_tx).await.unwrap();
        let host = Arc::new(host);

        let addr = format!("127.0.0.1:{port}");
        let mut stream1 = TcpStream::connect(&addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let tracker = Arc::new(Mutex::new(HostClockTracker::new()));
        let scheduler = PlaybackScheduler::new(tracker, host.clone(), None, 44100);

        scheduler.pause(12345).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let msg = read_message(&mut stream1).await.unwrap();

        match msg {
            Message::PauseCommand { position_samples, sample_rate } => {
                assert_eq!(position_samples, 12345);
                assert_eq!(sample_rate, 44100);
            }
            other => panic!("Expected PauseCommand, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_scheduler_broadcasts_stop_immediately() {
        use crate::network::messages::{read_message, Message};
        use crate::network::tcp::TcpHost;
        use tokio::net::TcpStream;
        use tokio::sync::mpsc;

        let port = free_port().await;
        let (event_tx, _event_rx) = mpsc::channel(64);
        let host = TcpHost::start(port, event_tx).await.unwrap();
        let host = Arc::new(host);

        let addr = format!("127.0.0.1:{port}");
        let mut stream1 = TcpStream::connect(&addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let tracker = Arc::new(Mutex::new(HostClockTracker::new()));
        let scheduler = PlaybackScheduler::new(tracker, host.clone(), None, 44100);

        scheduler.stop().await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let msg = read_message(&mut stream1).await.unwrap();

        assert_eq!(msg, Message::StopCommand);
    }
}
