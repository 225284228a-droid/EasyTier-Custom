use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use super::bandwidth::TransmissionWindowSource;

const PEER_WINDOW_REPORT_TTL: Duration = Duration::from_secs(90);

pub struct WindowLatency {
    latency_us_window: Vec<AtomicU32>,
    latency_us_window_index: AtomicU32,
    latency_us_window_size: u32,
    sum: AtomicU32,
    count: AtomicU32,
}

impl std::fmt::Debug for WindowLatency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowLatency")
            .field("count", &self.count)
            .field("window_size", &self.latency_us_window_size)
            .field("window_latency", &self.get_latency_us::<u32>())
            .finish()
    }
}

impl WindowLatency {
    pub fn new(window_size: u32) -> Self {
        Self {
            latency_us_window: (0..window_size).map(|_| AtomicU32::new(0)).collect(),
            latency_us_window_index: AtomicU32::new(0),
            latency_us_window_size: window_size,
            sum: AtomicU32::new(0),
            count: AtomicU32::new(0),
        }
    }

    pub fn record_latency(&self, latency_us: u32) {
        let index = self.latency_us_window_index.fetch_add(1, Relaxed);
        if self.count.load(Relaxed) < self.latency_us_window_size {
            self.count.fetch_add(1, Relaxed);
        }
        let index = index % self.latency_us_window_size;
        let old_lat = self.latency_us_window[index as usize].swap(latency_us, Relaxed);
        if old_lat < latency_us {
            self.sum.fetch_add(latency_us - old_lat, Relaxed);
        } else {
            self.sum.fetch_sub(old_lat - latency_us, Relaxed);
        }
    }

    pub fn get_latency_us<T: From<u32> + std::ops::Div<Output = T>>(&self) -> T {
        let count = self.count.load(Relaxed);
        let sum = self.sum.load(Relaxed);
        if count == 0 {
            0.into()
        } else {
            T::from(sum) / T::from(count)
        }
    }
}

#[derive(Debug, Default)]
pub struct Throughput {
    tx_bytes: AtomicU64,
    rx_bytes: AtomicU64,
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    window_source: Mutex<Option<Arc<dyn TransmissionWindowSource>>>,
    peer_window_report: Mutex<Option<(u64, Instant)>>,
}

impl Clone for Throughput {
    fn clone(&self) -> Self {
        Self {
            tx_bytes: AtomicU64::new(self.tx_bytes()),
            rx_bytes: AtomicU64::new(self.rx_bytes()),
            tx_packets: AtomicU64::new(self.tx_packets()),
            rx_packets: AtomicU64::new(self.rx_packets()),
            ..Default::default()
        }
    }
}

impl Throughput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tx_bytes(&self) -> u64 {
        self.tx_bytes.load(Relaxed)
    }

    pub fn rx_bytes(&self) -> u64 {
        self.rx_bytes.load(Relaxed)
    }

    pub fn tx_packets(&self) -> u64 {
        self.tx_packets.load(Relaxed)
    }

    pub fn rx_packets(&self) -> u64 {
        self.rx_packets.load(Relaxed)
    }

    pub fn record_tx_bytes(&self, bytes: u64) {
        self.tx_bytes.fetch_add(bytes, Relaxed);
        self.tx_packets.fetch_add(1, Relaxed);
    }

    pub fn record_rx_bytes(&self, bytes: u64) {
        self.rx_bytes.fetch_add(bytes, Relaxed);
        self.rx_packets.fetch_add(1, Relaxed);
    }

    pub(crate) fn set_window_source(&self, source: Option<Arc<dyn TransmissionWindowSource>>) {
        *self.window_source.lock().unwrap() = source;
    }

    pub(crate) fn record_peer_window_estimate(&self, bps: u64) {
        *self.peer_window_report.lock().unwrap() = (bps > 0).then(|| (bps, Instant::now()));
    }

    /// The live transport's real sending-window/RTT estimate, in bit/s.
    /// No payload counters, socket buffer sizes or traffic-rate fallback are used.
    pub fn estimated_tx_bps(&self) -> u64 {
        self.window_source
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|source| source.transmission_window())
            .and_then(|window| window.estimated_bps())
            .unwrap_or_default()
    }

    pub fn estimated_rx_bps(&self) -> u64 {
        self.peer_estimate_at(Instant::now())
    }

    fn peer_estimate_at(&self, now: Instant) -> u64 {
        self.peer_window_report
            .lock()
            .unwrap()
            .filter(|(_, observed_at)| {
                now.saturating_duration_since(*observed_at) < PEER_WINDOW_REPORT_TTL
            })
            .map(|(rate, _)| rate)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::bandwidth::TransmissionWindow;

    #[derive(Debug)]
    struct TestWindowSource(Mutex<Option<TransmissionWindow>>);

    impl TransmissionWindowSource for TestWindowSource {
        fn transmission_window(&self) -> Option<TransmissionWindow> {
            *self.0.lock().unwrap()
        }
    }

    #[test]
    fn empty_business_traffic_still_has_a_real_window_estimate() {
        let throughput = Throughput::new();
        throughput.set_window_source(Some(Arc::new(TestWindowSource(Mutex::new(Some(
            TransmissionWindow {
                congestion_window_bytes: 1_000_000,
                peer_receive_window_bytes: Some(250_000),
                rtt: Duration::from_millis(50),
            },
        ))))));
        assert_eq!(throughput.tx_bytes(), 0);
        assert_eq!(throughput.estimated_tx_bps(), 40_000_000);
        assert_eq!(throughput.estimated_rx_bps(), 0);
    }

    #[test]
    fn counters_never_create_or_override_bandwidth_estimates() {
        let throughput = Throughput::new();
        for _ in 0..100 {
            throughput.record_tx_bytes(20_000);
            throughput.record_rx_bytes(30_000);
        }
        assert_eq!(throughput.tx_bytes(), 2_000_000);
        assert_eq!(throughput.rx_bytes(), 3_000_000);
        assert_eq!(throughput.tx_packets(), 100);
        assert_eq!(throughput.rx_packets(), 100);
        assert_eq!(throughput.estimated_tx_bps(), 0);
        assert_eq!(throughput.estimated_rx_bps(), 0);
    }

    #[test]
    fn live_window_changes_are_used_without_new_payload_samples() {
        let throughput = Throughput::new();
        let window = TransmissionWindow {
            congestion_window_bytes: 125_000,
            peer_receive_window_bytes: None,
            rtt: Duration::from_millis(10),
        };
        let source = Arc::new(TestWindowSource(Mutex::new(Some(window))));
        throughput.set_window_source(Some(source.clone()));
        assert_eq!(throughput.estimated_tx_bps(), 100_000_000);
        *source.0.lock().unwrap() = Some(TransmissionWindow {
            rtt: Duration::from_millis(20),
            ..window
        });
        assert_eq!(throughput.estimated_tx_bps(), 50_000_000);
        *source.0.lock().unwrap() = None;
        assert_eq!(throughput.estimated_tx_bps(), 0);
    }

    #[test]
    fn download_comes_from_the_peer_not_a_mirrored_upload_value() {
        let throughput = Throughput::new();
        throughput.record_peer_window_estimate(80_000_000);
        assert_eq!(throughput.estimated_rx_bps(), 80_000_000);
        assert_eq!(throughput.estimated_tx_bps(), 0);
        throughput.record_peer_window_estimate(0);
        assert_eq!(throughput.estimated_rx_bps(), 0);
    }

    #[test]
    fn polling_does_not_extend_the_remote_report_lifetime() {
        let throughput = Throughput::new();
        let now = Instant::now();
        *throughput.peer_window_report.lock().unwrap() = Some((80_000_000, now));
        assert_eq!(
            throughput.peer_estimate_at(now + Duration::from_secs(89)),
            80_000_000
        );
        assert_eq!(throughput.peer_estimate_at(now + PEER_WINDOW_REPORT_TTL), 0);
    }

    #[test]
    fn liveness_counter_snapshots_do_not_keep_transport_telemetry_alive() {
        let throughput = Throughput::new();
        throughput.record_tx_bytes(100);
        throughput.record_peer_window_estimate(80_000_000);
        let snapshot = throughput.clone();
        assert_eq!(snapshot.tx_bytes(), 100);
        assert_eq!(snapshot.estimated_rx_bps(), 0);
        assert_eq!(snapshot.estimated_tx_bps(), 0);
    }
}
