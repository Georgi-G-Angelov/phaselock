use std::collections::{HashMap, VecDeque};
use std::time::Instant;

// ── Constants ───────────────────────────────────────────────────────────────

/// Maximum number of measurements in the sliding window.
const WINDOW_SIZE: usize = 10;

/// Measurements older than this are considered stale and discarded.
const STALE_THRESHOLD_SECS: f64 = 2.0;

// ── ClockSync (peer side) ───────────────────────────────────────────────────

/// A single RTT + offset measurement.
#[derive(Debug, Clone)]
struct ClockMeasurement {
    timestamp: Instant,
    rtt_ns: u64,
    offset_ns: i64,
}

/// Peer-side clock synchronization state.
///
/// Maintains a sliding window of NTP-style measurements and computes a
/// median-filtered estimate of the clock offset relative to the host.
#[derive(Debug)]
pub struct ClockSync {
    window: VecDeque<ClockMeasurement>,
    /// Current best estimate: `peer_clock − host_clock` in nanoseconds.
    pub current_offset_ns: i64,
    /// Estimated one-way network latency in nanoseconds.
    pub current_latency_ns: u64,
}

impl ClockSync {
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW_SIZE),
            current_offset_ns: 0,
            current_latency_ns: 0,
        }
    }

    /// Record a new measurement from a ClockPong response.
    ///
    /// All timestamp arguments are in nanoseconds from their respective
    /// monotonic clocks.
    pub fn record_pong(
        &mut self,
        peer_send_time_ns: u64,
        host_recv_time_ns: u64,
        host_send_time_ns: u64,
        peer_recv_time_ns: u64,
    ) {
        let host_processing = host_send_time_ns.wrapping_sub(host_recv_time_ns) as i64;
        let total_elapsed = peer_recv_time_ns.wrapping_sub(peer_send_time_ns) as i64;
        let rtt_ns = (total_elapsed - host_processing).max(0) as u64;

        // NTP offset formula:
        // offset = ((host_recv - peer_send) + (host_send - peer_recv)) / 2
        let a = host_recv_time_ns as i64 - peer_send_time_ns as i64;
        let b = host_send_time_ns as i64 - peer_recv_time_ns as i64;
        let offset_ns = (a + b) / 2;

        self.window.push_back(ClockMeasurement {
            timestamp: Instant::now(),
            rtt_ns,
            offset_ns,
        });

        // Evict old/stale entries.
        self.prune_stale();

        // Cap at WINDOW_SIZE.
        while self.window.len() > WINDOW_SIZE {
            self.window.pop_front();
        }

        // Recompute estimates.
        self.recompute();
    }

    /// Record a measurement using a pre-set `Instant` (for testing).
    #[cfg(test)]
    fn record_measurement(&mut self, rtt_ns: u64, offset_ns: i64, timestamp: Instant) {
        self.window.push_back(ClockMeasurement {
            timestamp,
            rtt_ns,
            offset_ns,
        });
        while self.window.len() > WINDOW_SIZE {
            self.window.pop_front();
        }
        self.recompute();
    }

    /// Remove measurements older than `STALE_THRESHOLD_SECS`.
    fn prune_stale(&mut self) {
        let now = Instant::now();
        self.window
            .retain(|m| now.duration_since(m.timestamp).as_secs_f64() < STALE_THRESHOLD_SECS);
    }

    /// Recompute offset and latency from the current window using median.
    fn recompute(&mut self) {
        if self.window.is_empty() {
            return;
        }
        self.current_offset_ns = median_i64(self.window.iter().map(|m| m.offset_ns));
        let median_rtt = median_u64(self.window.iter().map(|m| m.rtt_ns));
        self.current_latency_ns = median_rtt / 2;
    }

    /// Convert a host-clock timestamp (ns) to this peer's local clock.
    pub fn host_to_local(&self, host_time_ns: u64) -> u64 {
        // local = host + offset  (offset = local − host)
        (host_time_ns as i64 + self.current_offset_ns) as u64
    }

    /// Convert a local-clock timestamp (ns) to the host's clock.
    pub fn local_to_host(&self, local_time_ns: u64) -> u64 {
        // host = local − offset
        (local_time_ns as i64 - self.current_offset_ns) as u64
    }

    /// Number of measurements currently in the window.
    pub fn measurement_count(&self) -> usize {
        self.window.len()
    }
}

impl Default for ClockSync {
    fn default() -> Self {
        Self::new()
    }
}

// ── HostClockTracker ────────────────────────────────────────────────────────

/// Per-peer latency information tracked on the host side.
#[derive(Debug, Clone)]
pub struct PeerLatency {
    pub estimated_one_way_ns: u64,
    pub last_updated: Instant,
}

/// Host-side tracker that maintains per-peer latency estimates.
#[derive(Debug)]
pub struct HostClockTracker {
    peer_latencies: HashMap<u32, PeerLatency>,
}

impl HostClockTracker {
    pub fn new() -> Self {
        Self {
            peer_latencies: HashMap::new(),
        }
    }

    /// Record a ping/pong round for a peer.
    ///
    /// `host_recv_ns` and `host_send_ns` are the host's monotonic timestamps
    /// when it received the ping and sent the pong.
    /// `peer_send_ns` is the peer's send timestamp from the `ClockPing`.
    pub fn record_ping(
        &mut self,
        peer_id: u32,
        _peer_send_ns: u64,
        host_recv_ns: u64,
        host_send_ns: u64,
    ) {
        // On the host side we only know the host-side processing time.
        // We approximate one-way latency as half the processing + propagation
        // that we can observe. The full RTT is known only to the peer.
        // We store host processing time / 2 as a minimum baseline; the peer
        // supplies the full RTT-based estimate via its ClockSync.
        // For command dispatch staggering we use the peer's reported latency,
        // but we also keep a local approximation:
        let processing_ns = host_send_ns.saturating_sub(host_recv_ns);
        self.peer_latencies.insert(
            peer_id,
            PeerLatency {
                estimated_one_way_ns: processing_ns / 2,
                last_updated: Instant::now(),
            },
        );
    }

    /// Get a peer's estimated one-way latency in nanoseconds.
    pub fn get_latency(&self, peer_id: u32) -> Option<&PeerLatency> {
        self.peer_latencies.get(&peer_id)
    }

    /// Remove a peer (e.g. on disconnect).
    pub fn remove_peer(&mut self, peer_id: u32) {
        self.peer_latencies.remove(&peer_id);
    }
}

impl Default for HostClockTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ── Median helpers ──────────────────────────────────────────────────────────

fn median_i64(iter: impl Iterator<Item = i64>) -> i64 {
    let mut vals: Vec<i64> = iter.collect();
    vals.sort_unstable();
    if vals.is_empty() {
        return 0;
    }
    vals[vals.len() / 2]
}

fn median_u64(iter: impl Iterator<Item = u64>) -> u64 {
    let mut vals: Vec<u64> = iter.collect();
    vals.sort_unstable();
    if vals.is_empty() {
        return 0;
    }
    vals[vals.len() / 2]
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_offset_calculation_known_values() {
        let mut cs = ClockSync::new();

        // Simulated scenario:
        // peer_send = 1000, host_recv = 2010, host_send = 2020, peer_recv = 3000
        // RTT = (3000-1000) - (2020-2010) = 2000 - 10 = 1990
        // offset = ((2010-1000) + (2020-3000)) / 2 = (1010 + (-980)) / 2 = 15
        cs.record_pong(1000, 2010, 2020, 3000);

        assert_eq!(cs.window.len(), 1);
        assert_eq!(cs.window[0].rtt_ns, 1990);
        assert_eq!(cs.window[0].offset_ns, 15);
        assert_eq!(cs.current_offset_ns, 15);
        assert_eq!(cs.current_latency_ns, 1990 / 2);
    }

    #[test]
    fn test_symmetric_scenario_zero_offset() {
        let mut cs = ClockSync::new();

        // Symmetric: peer_send=0, host_recv=500, host_send=500, peer_recv=1000
        // RTT = (1000-0) - (500-500) = 1000
        // offset = ((500-0) + (500-1000)) / 2 = (500 + (-500)) / 2 = 0
        cs.record_pong(0, 500, 500, 1000);

        assert_eq!(cs.current_offset_ns, 0);
        assert_eq!(cs.current_latency_ns, 500);
    }

    #[test]
    fn test_sliding_window_median() {
        let mut cs = ClockSync::new();
        let now = Instant::now();

        // Insert 10 measurements with offsets: [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]
        for i in 1..=10 {
            cs.record_measurement(100, i * 10, now);
        }

        assert_eq!(cs.measurement_count(), 10);

        // Sorted offsets: [10,20,30,40,50,60,70,80,90,100], median index 5 → 60
        assert_eq!(cs.current_offset_ns, 60);
    }

    #[test]
    fn test_window_cap_at_10() {
        let mut cs = ClockSync::new();
        let now = Instant::now();

        // Insert 15 measurements — window should keep last 10.
        for i in 0..15 {
            cs.record_measurement(100, i * 10, now);
        }

        assert_eq!(cs.measurement_count(), 10);
    }

    #[test]
    fn test_stale_measurements_discarded() {
        let mut cs = ClockSync::new();

        // Insert a measurement manually with an old timestamp.
        let old_time = Instant::now() - Duration::from_secs(5);
        cs.window.push_back(ClockMeasurement {
            timestamp: old_time,
            rtt_ns: 1000,
            offset_ns: 999,
        });
        cs.recompute();
        assert_eq!(cs.current_offset_ns, 999);

        // Now add a fresh measurement — prune_stale should remove the old one.
        cs.record_pong(0, 500, 500, 1000);

        assert_eq!(cs.measurement_count(), 1);
        // The stale measurement (offset 999) should be gone; only fresh one remains.
        assert_eq!(cs.current_offset_ns, 0);
    }

    #[test]
    fn test_outlier_filtered_by_median() {
        let mut cs = ClockSync::new();
        let now = Instant::now();

        // 9 measurements with offset ≈ 100, 1 outlier at 10000.
        for _ in 0..9 {
            cs.record_measurement(100, 100, now);
        }
        cs.record_measurement(100, 10000, now);

        // Sorted: [100,100,100,100,100,100,100,100,100,10000], median index 5 → 100
        assert_eq!(cs.current_offset_ns, 100);
    }

    #[test]
    fn test_host_to_local_and_local_to_host_are_inverses() {
        let mut cs = ClockSync::new();
        let now = Instant::now();

        // Set offset to +500 (peer clock is 500ns ahead of host).
        cs.record_measurement(100, 500, now);

        let host_time: u64 = 1_000_000_000;
        let local = cs.host_to_local(host_time);
        let back = cs.local_to_host(local);

        assert_eq!(back, host_time);
    }

    #[test]
    fn test_host_to_local_positive_offset() {
        let mut cs = ClockSync::new();
        let now = Instant::now();
        cs.record_measurement(100, 1000, now);

        // offset = 1000, so local = host + 1000
        assert_eq!(cs.host_to_local(5000), 6000);
    }

    #[test]
    fn test_local_to_host_positive_offset() {
        let mut cs = ClockSync::new();
        let now = Instant::now();
        cs.record_measurement(100, 1000, now);

        // offset = 1000, so host = local - 1000
        assert_eq!(cs.local_to_host(6000), 5000);
    }

    #[test]
    fn test_host_clock_tracker() {
        let mut tracker = HostClockTracker::new();

        tracker.record_ping(1, 100, 200, 210);
        let lat = tracker.get_latency(1).unwrap();
        assert_eq!(lat.estimated_one_way_ns, 5); // (210-200)/2

        tracker.remove_peer(1);
        assert!(tracker.get_latency(1).is_none());
    }

    #[test]
    fn test_empty_clock_sync() {
        let cs = ClockSync::new();
        assert_eq!(cs.current_offset_ns, 0);
        assert_eq!(cs.current_latency_ns, 0);
        assert_eq!(cs.measurement_count(), 0);
    }
}
