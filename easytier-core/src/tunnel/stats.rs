use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use crate::foundation::bandwidth::{TrafficRateReport, TransmissionWindowSource};

const PEER_WINDOW_REPORT_TTL: Duration = Duration::from_secs(90);
const RATE_SAMPLE_MIN_INTERVAL: Duration = Duration::from_millis(500);
const RATE_SAMPLE_MAX_INTERVAL: Duration = Duration::from_secs(5);
const RATE_SAMPLE_TTL: Duration = Duration::from_secs(5);
const RATE_SAMPLE_MIN_BYTES: u64 = 4 * 1024;

const DELIVERY_SAMPLE_TTL: Duration = Duration::from_secs(10);
const DELIVERY_SAMPLE_LIMIT: usize = 20;
const DELIVERY_MIN_BPS: u64 = 64_000;

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

#[derive(Debug, Clone, Copy)]
struct RateSnapshot {
    observed_at: Instant,
    report: TrafficRateReport,
}

#[derive(Debug)]
struct RateState {
    baseline_at: Instant,
    tx_bytes: u64,
    rx_bytes: u64,
    last: Option<RateSnapshot>,
}

impl Default for RateState {
    fn default() -> Self {
        Self {
            baseline_at: Instant::now(),
            tx_bytes: 0,
            rx_bytes: 0,
            last: None,
        }
    }
}

/// Recent delivery-rate samples of one direction. A sample cannot exceed
/// what the path delivers, but it understates it whenever the sender had less
/// to send, so the recent maximum is the estimate (as BBR's bandwidth
/// filter). A path that stops carrying traffic ages out to unknown.
#[derive(Debug, Default)]
struct DeliveryRate {
    samples: VecDeque<(Instant, u64)>,
}

impl DeliveryRate {
    fn record(&mut self, delivered_bps: u64, sample_window: Duration, now: Instant) {
        if sample_window < RATE_SAMPLE_MIN_INTERVAL
            || delivered_bps == u64::MAX
            || delivered_bps < DELIVERY_MIN_BPS
        {
            return;
        }
        self.samples.push_back((now, delivered_bps));
        while self.samples.len() > DELIVERY_SAMPLE_LIMIT {
            self.samples.pop_front();
        }
        self.retain_recent(now);
    }

    fn estimate(&mut self, now: Instant) -> u64 {
        self.retain_recent(now);
        self.samples
            .iter()
            .map(|(_, rate)| *rate)
            .max()
            .unwrap_or_default()
    }

    fn retain_recent(&mut self, now: Instant) {
        self.samples.retain(|(observed_at, _)| {
            now.saturating_duration_since(*observed_at) < DELIVERY_SAMPLE_TTL
        });
    }
}

#[derive(Debug, Default)]
struct DeliveryState {
    tx: DeliveryRate,
    rx: DeliveryRate,
}

#[derive(Debug, Default)]
pub struct Throughput {
    tx_bytes: AtomicU64,
    rx_bytes: AtomicU64,
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    window_source: Mutex<Option<Arc<dyn TransmissionWindowSource>>>,
    peer_window_report: Mutex<Option<(u64, Instant)>>,
    rate_state: Mutex<RateState>,
    delivery: Mutex<DeliveryState>,
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
        if bps > 0 && bps < u64::MAX {
            *self.peer_window_report.lock().unwrap() = Some((bps, Instant::now()));
        }
    }

    pub(crate) fn record_peer_traffic_report(
        &self,
        version: u32,
        peer_rx_bps: u64,
        peer_estimated_tx_bps: u64,
        peer_sample_window: Duration,
    ) {
        self.record_peer_window_estimate(peer_estimated_tx_bps);
        if version < 2 {
            return;
        }

        let now = Instant::now();
        let local = self.traffic_rate_at(now);
        let window = peer_sample_window.min(local.sample_window);
        let mut delivery = self.delivery.lock().unwrap();
        // The peer measures what reached it of our traffic; we measure what
        // reached us of its traffic.
        delivery.tx.record(peer_rx_bps, window, now);
        delivery.rx.record(local.rx_bps, window, now);
    }

    pub(crate) fn traffic_report(&self) -> TrafficRateReport {
        self.traffic_report_at(Instant::now())
    }

    fn traffic_report_at(&self, now: Instant) -> TrafficRateReport {
        let mut report = self.traffic_rate_at(now);
        report.estimated_tx_bps = self.estimated_tx_bps_at(now);
        report
    }

    /// Upload capacity in bit/s: the recent maximum delivery rate the peer
    /// reported, or, without delivery samples (an idle path, or a peer that
    /// only sends v1 reports), the live transport's window over its own RTT.
    /// Windowless transports without samples stay unknown.
    ///
    /// When a CDN terminates the transport, the window describes only the
    /// measured transport leg; delivery samples cover the whole path.
    pub fn estimated_tx_bps(&self) -> u64 {
        self.estimated_tx_bps_at(Instant::now())
    }

    /// The upload rate measured from delivery alone; zero until the peer has
    /// reported recent delivery of this path's traffic.
    pub fn delivered_tx_bps(&self) -> u64 {
        self.delivery.lock().unwrap().tx.estimate(Instant::now())
    }

    fn estimated_tx_bps_at(&self, now: Instant) -> u64 {
        let delivered = self.delivery.lock().unwrap().tx.estimate(now);
        if delivered > 0 {
            return delivered;
        }
        self.window_source
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|source| source.transmission_window())
            .and_then(|window| window.estimated_bps())
            .unwrap_or_default()
    }

    /// Download capacity: the recent maximum rate this end received, or the
    /// peer's own upload estimate while nothing was measured locally.
    pub fn estimated_rx_bps(&self) -> u64 {
        let now = Instant::now();
        let delivered = self.delivery.lock().unwrap().rx.estimate(now);
        if delivered > 0 {
            delivered
        } else {
            self.peer_estimate_at(now)
        }
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

    fn traffic_rate_at(&self, now: Instant) -> TrafficRateReport {
        let mut state = self.rate_state.lock().unwrap();
        let elapsed = now.saturating_duration_since(state.baseline_at);
        if elapsed < RATE_SAMPLE_MIN_INTERVAL {
            return state
                .last
                .filter(|sample| {
                    now.saturating_duration_since(sample.observed_at) <= RATE_SAMPLE_TTL
                })
                .map(|sample| sample.report)
                .unwrap_or_default();
        }

        let tx_bytes = self.tx_bytes();
        let rx_bytes = self.rx_bytes();
        let tx_delta = tx_bytes.saturating_sub(state.tx_bytes);
        let rx_delta = rx_bytes.saturating_sub(state.rx_bytes);
        state.baseline_at = now;
        state.tx_bytes = tx_bytes;
        state.rx_bytes = rx_bytes;

        if elapsed > RATE_SAMPLE_MAX_INTERVAL {
            state.last = None;
            return TrafficRateReport::default();
        }

        let rate = |bytes: u64| {
            if bytes < RATE_SAMPLE_MIN_BYTES {
                0
            } else {
                (u128::from(bytes) * 8 * 1_000_000_000 / elapsed.as_nanos())
                    .try_into()
                    .ok()
                    .filter(|rate| *rate < u64::MAX)
                    .unwrap_or(0)
            }
        };
        let report = TrafficRateReport {
            estimated_tx_bps: 0,
            tx_bps: rate(tx_delta),
            rx_bps: rate(rx_delta),
            sample_window: elapsed,
        };
        state.last = Some(RateSnapshot {
            observed_at: now,
            report,
        });
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundation::bandwidth::{CongestionControl, TransmissionWindow};

    #[derive(Debug)]
    struct TestWindowSource(Mutex<Option<TransmissionWindow>>);

    impl TransmissionWindowSource for TestWindowSource {
        fn transmission_window(&self) -> Option<TransmissionWindow> {
            *self.0.lock().unwrap()
        }
    }

    fn seed_rates(
        throughput: &Throughput,
        now: Instant,
        tx_bps: u64,
        rx_bps: u64,
    ) -> TrafficRateReport {
        let report = TrafficRateReport {
            estimated_tx_bps: 0,
            tx_bps,
            rx_bps,
            sample_window: Duration::from_secs(1),
        };
        let mut state = throughput.rate_state.lock().unwrap();
        state.baseline_at = now;
        state.tx_bytes = throughput.tx_bytes();
        state.rx_bytes = throughput.rx_bytes();
        state.last = Some(RateSnapshot {
            observed_at: now,
            report,
        });
        report
    }

    #[test]
    fn empty_business_traffic_still_has_a_real_window_estimate() {
        let throughput = Throughput::new();
        throughput.set_window_source(Some(Arc::new(TestWindowSource(Mutex::new(Some(
            TransmissionWindow {
                congestion_window_bytes: 1_000_000,
                peer_receive_window_bytes: Some(250_000),
                rtt: Duration::from_millis(50),
                congestion_control: CongestionControl::LossBased,
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
    fn idle_windowless_transport_stays_unknown() {
        let throughput = Throughput::new();
        assert_eq!(throughput.estimated_tx_bps(), 0);
        assert_eq!(throughput.estimated_rx_bps(), 0);
    }

    #[test]
    fn delivery_samples_take_precedence_over_the_window() {
        let throughput = Throughput::new();
        throughput.set_window_source(Some(Arc::new(TestWindowSource(Mutex::new(Some(
            TransmissionWindow {
                congestion_window_bytes: 1_000_000,
                peer_receive_window_bytes: None,
                rtt: Duration::from_millis(50),
                congestion_control: CongestionControl::LossBased,
            },
        ))))));
        assert_eq!(throughput.estimated_tx_bps(), 160_000_000);
        assert_eq!(throughput.delivered_tx_bps(), 0);
        let now = Instant::now();
        throughput
            .delivery
            .lock()
            .unwrap()
            .tx
            .record(30_000_000, Duration::from_secs(1), now);
        assert_eq!(throughput.estimated_tx_bps_at(now), 30_000_000);
        assert_eq!(throughput.delivered_tx_bps(), 30_000_000);
        // Stale samples fall back to the window.
        assert_eq!(
            throughput.estimated_tx_bps_at(now + DELIVERY_SAMPLE_TTL),
            160_000_000
        );
    }

    #[test]
    fn a_delivery_report_creates_an_upload_estimate() {
        let throughput = Throughput::new();
        throughput.record_tx_bytes(12_500_000);
        seed_rates(&throughput, Instant::now(), 100_000_000, 0);
        throughput.record_peer_traffic_report(2, 10_000_000, 0, Duration::from_secs(1));
        assert_eq!(throughput.estimated_tx_bps(), 10_000_000);
        assert_eq!(throughput.estimated_rx_bps(), 0);
    }

    #[test]
    fn locally_received_traffic_creates_a_download_estimate() {
        let throughput = Throughput::new();
        throughput.record_rx_bytes(12_500_000);
        seed_rates(&throughput, Instant::now(), 0, 10_000_000);
        throughput.record_peer_window_estimate(80_000_000);
        throughput.record_peer_traffic_report(2, 0, 0, Duration::from_secs(1));
        assert_eq!(throughput.estimated_rx_bps(), 10_000_000);
        assert_eq!(throughput.estimated_tx_bps(), 0);
    }

    #[test]
    fn the_recent_maximum_survives_lighter_traffic() {
        let now = Instant::now();
        let mut rate = DeliveryRate::default();
        let second = Duration::from_secs(1);
        rate.record(50_000_000, second, now);
        rate.record(10_000_000, second, now + second);
        assert_eq!(rate.estimate(now + second), 50_000_000);
        // Too short a window, too little traffic or an overflowed value is
        // not a sample.
        rate.record(90_000_000, Duration::from_millis(100), now + second);
        rate.record(DELIVERY_MIN_BPS - 1, second, now + second);
        rate.record(u64::MAX, second, now + second);
        assert_eq!(rate.estimate(now + second), 50_000_000);
        assert_eq!(rate.estimate(now + DELIVERY_SAMPLE_TTL), 10_000_000);
        assert_eq!(rate.estimate(now + second + DELIVERY_SAMPLE_TTL), 0);
    }

    #[test]
    fn v1_reports_only_carry_the_peer_window() {
        let throughput = Throughput::new();
        throughput.record_rx_bytes(12_500_000);
        seed_rates(&throughput, Instant::now(), 100_000_000, 10_000_000);
        throughput.record_peer_traffic_report(1, 10_000_000, 40_000_000, Duration::from_secs(1));
        assert_eq!(throughput.delivered_tx_bps(), 0);
        assert_eq!(throughput.estimated_rx_bps(), 40_000_000);
    }

    #[test]
    fn live_window_changes_are_used_without_new_payload_samples() {
        let throughput = Throughput::new();
        let window = TransmissionWindow {
            congestion_window_bytes: 125_000,
            peer_receive_window_bytes: None,
            rtt: Duration::from_millis(10),
            congestion_control: CongestionControl::LossBased,
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
        assert_eq!(throughput.estimated_rx_bps(), 80_000_000);
    }

    #[test]
    fn missing_reports_do_not_refresh_or_erase_the_last_valid_estimate() {
        let throughput = Throughput::new();
        throughput.record_peer_window_estimate(0);
        throughput.record_peer_window_estimate(u64::MAX);
        assert_eq!(throughput.estimated_rx_bps(), 0);
        let now = Instant::now();
        *throughput.peer_window_report.lock().unwrap() = Some((80_000_000, now));
        throughput.record_peer_window_estimate(0);
        throughput.record_peer_window_estimate(u64::MAX);
        assert_eq!(
            throughput.peer_estimate_at(now + Duration::from_secs(89)),
            80_000_000
        );
        assert_eq!(throughput.peer_estimate_at(now + PEER_WINDOW_REPORT_TTL), 0);
    }

    #[test]
    fn sub_millisecond_lan_estimate_is_independent_of_application_latency() {
        let throughput = Throughput::new();
        throughput.set_window_source(Some(Arc::new(TestWindowSource(Mutex::new(Some(
            TransmissionWindow {
                congestion_window_bytes: 25_000,
                peer_receive_window_bytes: None,
                rtt: Duration::from_micros(200),
                congestion_control: CongestionControl::LossBased,
            },
        ))))));
        let latency = WindowLatency::new(10);
        for ping_us in [4_000, 50_000, 90_000] {
            latency.record_latency(ping_us);
            assert_eq!(throughput.estimated_tx_bps(), 1_000_000_000);
        }
        // The same transport snapshot works before and during a busy upload.
        throughput.record_tx_bytes(125_000_000);
        assert_eq!(throughput.estimated_tx_bps(), 1_000_000_000);
        assert_eq!(latency.get_latency_us::<u32>(), 48_000);
        throughput.set_window_source(None);
        assert_eq!(throughput.estimated_tx_bps(), 0);
    }

    #[test]
    fn loss_based_window_uses_the_transport_rtt() {
        let throughput = Throughput::new();
        throughput.set_window_source(Some(Arc::new(TestWindowSource(Mutex::new(Some(
            TransmissionWindow {
                congestion_window_bytes: 125_000,
                peer_receive_window_bytes: None,
                rtt: Duration::from_millis(20),
                congestion_control: CongestionControl::LossBased,
            },
        ))))));
        assert_eq!(throughput.estimated_tx_bps(), 50_000_000);
    }

    #[test]
    fn forwarded_bbr_window_describes_only_its_measured_transport_leg() {
        let throughput = Throughput::new();
        throughput.set_window_source(Some(Arc::new(TestWindowSource(Mutex::new(Some(
            TransmissionWindow {
                congestion_window_bytes: 5_000_000,
                peer_receive_window_bytes: None,
                rtt: Duration::from_millis(20),
                congestion_control: CongestionControl::Bbr,
            },
        ))))));
        // 2.5 MB of effective window over this leg's 20 ms RTT: 1 Gbit/s.
        assert_eq!(throughput.estimated_tx_bps(), 1_000_000_000);
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
