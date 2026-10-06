use std::{
    sync::{
        Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

const BANDWIDTH_WINDOW: Duration = Duration::from_secs(10);
const BANDWIDTH_BUCKET: Duration = Duration::from_millis(200);
const BANDWIDTH_BUCKET_COUNT: usize =
    (BANDWIDTH_WINDOW.as_millis() / BANDWIDTH_BUCKET.as_millis()) as usize;
const BANDWIDTH_MIN_BYTES: u64 = 4_096;
const BANDWIDTH_MIN_PACKETS: u32 = 4;
const BANDWIDTH_MIN_SPAN: Duration = Duration::from_millis(100);
const BANDWIDTH_FALLBACK_MIN_BYTES: u64 = 1_024;
const BANDWIDTH_ESTIMATE_TTL: Duration = Duration::from_secs(300);

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
            (T::from(sum)) / T::from(count)
        }
    }
}

#[derive(Debug)]
pub struct Throughput {
    tx_bytes: AtomicU64,
    rx_bytes: AtomicU64,
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    bandwidth: Mutex<BandwidthWindow>,
}

impl Clone for Throughput {
    fn clone(&self) -> Self {
        Self {
            tx_bytes: AtomicU64::new(self.tx_bytes()),
            rx_bytes: AtomicU64::new(self.rx_bytes()),
            tx_packets: AtomicU64::new(self.tx_packets()),
            rx_packets: AtomicU64::new(self.rx_packets()),
            // Clones are used as point-in-time counter snapshots by the
            // liveness controller; they must not share the live rate window.
            bandwidth: Mutex::new(BandwidthWindow::default()),
        }
    }
}

impl Default for Throughput {
    fn default() -> Self {
        Self {
            tx_bytes: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            bandwidth: Mutex::new(BandwidthWindow::default()),
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

    pub(crate) fn record_tx_data_bytes(&self, bytes: u64) {
        self.bandwidth
            .lock()
            .unwrap()
            .record(Instant::now(), bytes, 0);
    }

    pub(crate) fn record_rx_data_bytes(&self, bytes: u64) {
        self.bandwidth
            .lock()
            .unwrap()
            .record(Instant::now(), 0, bytes);
    }

    /// Returns the peak business-data rate in recent 200 ms windows, in bit/s.
    ///
    /// This passive capacity hint is based on observed payload traffic, not
    /// an active speed test or an acknowledgement-derived delivery rate.
    /// Sparse transfers use the average over observed payload samples, with
    /// a minimum 200 ms span. Sampling retains usable estimates for five
    /// minutes, independently of whether statistics are being queried.
    /// It does not measure unused headroom.
    pub fn estimated_tx_bps(&self) -> u64 {
        self.bandwidth
            .lock()
            .unwrap()
            .estimate(Instant::now(), true)
    }

    pub fn estimated_rx_bps(&self) -> u64 {
        self.bandwidth
            .lock()
            .unwrap()
            .estimate(Instant::now(), false)
    }
}

#[derive(Debug)]
struct BandwidthWindow {
    started_at: Instant,
    buckets: [BandwidthBucket; BANDWIDTH_BUCKET_COUNT],
    aggregate_sequence: Option<u64>,
    tx: DirectionBandwidth,
    rx: DirectionBandwidth,
}

#[derive(Debug, Clone, Copy, Default)]
struct BandwidthBucket {
    sequence: Option<u64>,
    tx: DirectionSample,
    rx: DirectionSample,
}

#[derive(Debug, Clone, Copy, Default)]
struct DirectionSample {
    bytes: u64,
    packets: u32,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct DirectionBandwidth {
    aggregate: DirectionSample,
    recent_peak: Option<(u64, Instant)>,
    retained_peak: Option<(u64, Instant)>,
    retained_sparse: Option<(u64, Instant)>,
}

impl DirectionBandwidth {
    fn reset_recent(&mut self) {
        self.aggregate = DirectionSample::default();
        self.recent_peak = None;
    }

    fn merge_peak(&mut self, sample: &DirectionSample) {
        let rate = sample.estimate();
        let Some(observed_at) = sample.last_at.filter(|_| rate > 0) else {
            return;
        };
        if self
            .recent_peak
            .is_none_or(|(peak, at)| rate > peak || (rate == peak && observed_at > at))
        {
            self.recent_peak = Some((rate, observed_at));
        }
    }

    fn aggregate_with(&mut self, sample: &DirectionSample) {
        self.aggregate.aggregate_with(sample);
        self.merge_peak(sample);
    }

    fn record(&mut self, now: Instant, bytes: u64, bucket: &DirectionSample) {
        if bytes == 0 {
            return;
        }
        self.aggregate.record(now, bytes);
        self.merge_peak(bucket);
        if let Some(peak) = self.recent_peak {
            self.retained_peak = Some(peak);
        }
        let rate = self.aggregate.aggregate_estimate();
        if let Some(observed_at) = self.aggregate.last_at.filter(|_| rate > 0) {
            self.retained_sparse = Some((rate, observed_at));
        }
    }

    fn retained_estimate(&self, now: Instant) -> u64 {
        [self.retained_peak, self.retained_sparse]
            .into_iter()
            .flatten()
            .find(|(_, observed_at)| {
                now.saturating_duration_since(*observed_at) < BANDWIDTH_ESTIMATE_TTL
            })
            .map(|(rate, _)| rate)
            .unwrap_or_default()
    }
}

impl DirectionSample {
    fn record(&mut self, now: Instant, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.packets = self.packets.saturating_add(1);
        self.first_at.get_or_insert(now);
        self.last_at = Some(now);
    }

    fn estimate(&self) -> u64 {
        let (Some(first), Some(last)) = (self.first_at, self.last_at) else {
            return 0;
        };
        if self.bytes < BANDWIDTH_MIN_BYTES
            || self.packets < BANDWIDTH_MIN_PACKETS
            || last.saturating_duration_since(first) < BANDWIDTH_MIN_SPAN
        {
            return 0;
        }

        ((u128::from(self.bytes) * 8 * 1_000_000_000) / BANDWIDTH_BUCKET.as_nanos())
            .min(u128::from(u64::MAX)) as u64
    }

    fn aggregate_with(&mut self, other: &Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.packets = self.packets.saturating_add(other.packets);
        self.first_at = match (self.first_at, other.first_at) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        self.last_at = match (self.last_at, other.last_at) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left, right) => left.or(right),
        };
    }

    fn aggregate_estimate(&self) -> u64 {
        let (Some(first), Some(last)) = (self.first_at, self.last_at) else {
            return 0;
        };
        if self.bytes < BANDWIDTH_FALLBACK_MIN_BYTES || self.packets == 0 {
            return 0;
        }
        let elapsed = last.saturating_duration_since(first).max(BANDWIDTH_BUCKET);
        ((u128::from(self.bytes) * 8 * 1_000_000_000) / elapsed.as_nanos())
            .min(u128::from(u64::MAX)) as u64
    }
}

impl Default for BandwidthWindow {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl BandwidthWindow {
    fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            buckets: [BandwidthBucket::default(); BANDWIDTH_BUCKET_COUNT],
            aggregate_sequence: None,
            tx: DirectionBandwidth::default(),
            rx: DirectionBandwidth::default(),
        }
    }

    fn sequence(&self, now: Instant) -> u64 {
        (now.saturating_duration_since(self.started_at).as_millis() / BANDWIDTH_BUCKET.as_millis())
            as u64
    }

    fn record(&mut self, now: Instant, tx_bytes: u64, rx_bytes: u64) {
        let sequence = self.sequence(now);
        if self.aggregate_sequence != Some(sequence) {
            // Rebuild only when the bucket changes; packet updates stay O(1).
            self.tx.reset_recent();
            self.rx.reset_recent();
            for bucket in &self.buckets {
                if Self::is_recent(bucket, sequence) {
                    self.tx.aggregate_with(&bucket.tx);
                    self.rx.aggregate_with(&bucket.rx);
                }
            }
            self.aggregate_sequence = Some(sequence);
        }
        let bucket = &mut self.buckets[sequence as usize % BANDWIDTH_BUCKET_COUNT];
        if bucket.sequence != Some(sequence) {
            *bucket = BandwidthBucket {
                sequence: Some(sequence),
                ..Default::default()
            };
        }
        bucket.tx.record(now, tx_bytes);
        bucket.rx.record(now, rx_bytes);
        self.tx.record(now, tx_bytes, &bucket.tx);
        self.rx.record(now, rx_bytes, &bucket.rx);
    }

    fn is_recent(bucket: &BandwidthBucket, sequence: u64) -> bool {
        bucket.sequence.is_some_and(|recorded| {
            recorded <= sequence && sequence - recorded < BANDWIDTH_BUCKET_COUNT as u64
        })
    }

    fn estimate(&self, now: Instant, tx: bool) -> u64 {
        // Sampling retains both confidence levels, so querying cannot create
        // or prolong an estimate and later sparse traffic is not discarded.
        (if tx { &self.tx } else { &self.rx }).retained_estimate(now)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn record_transfer(window: &mut BandwidthWindow, start: Instant, tx: u64, rx: u64) {
        for offset_ms in [0, 40, 80, 120] {
            window.record(start + Duration::from_millis(offset_ms), tx, rx);
        }
    }

    fn sampled_data_bytes(throughput: &Throughput) -> (u64, u64) {
        let bandwidth = throughput.bandwidth.lock().unwrap();
        (
            bandwidth
                .buckets
                .iter()
                .map(|bucket| bucket.tx.bytes)
                .sum::<u64>(),
            bandwidth
                .buckets
                .iter()
                .map(|bucket| bucket.rx.bytes)
                .sum::<u64>(),
        )
    }

    #[test]
    fn bandwidth_window_uses_recent_peak_not_average_throughput() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        record_transfer(&mut window, start, 2_000, 3_000);
        record_transfer(&mut window, start + Duration::from_secs(1), 1_500, 5_000);

        assert_eq!(
            window.estimate(start + Duration::from_secs(2), true),
            320_000
        );
        assert_eq!(
            window.estimate(start + Duration::from_secs(2), false),
            800_000
        );
    }

    #[test]
    fn bandwidth_window_discards_stale_buckets() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        record_transfer(&mut window, start, 2_000, 3_000);

        assert_eq!(
            window.estimate(start + Duration::from_secs(9), true),
            320_000
        );
        assert_eq!(window.estimate(start + BANDWIDTH_WINDOW, true), 320_000);
        assert_eq!(
            window.estimate(start + BANDWIDTH_ESTIMATE_TTL + BANDWIDTH_BUCKET, true),
            0
        );
        assert_eq!(
            window.estimate(start + BANDWIDTH_ESTIMATE_TTL + BANDWIDTH_BUCKET, false),
            0
        );
    }

    #[test]
    fn bandwidth_window_requires_sustained_samples_in_each_direction() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        record_transfer(&mut window, start, 2_000, 0);
        window.record(start, 0, 100_000);
        assert_eq!(window.estimate(start + BANDWIDTH_BUCKET, true), 320_000);
        assert!(window.estimate(start + BANDWIDTH_BUCKET, false) > 0);

        record_transfer(&mut window, start + BANDWIDTH_BUCKET, 0, 3_000);
        assert_eq!(
            window.estimate(start + BANDWIDTH_BUCKET * 2, false),
            480_000
        );
    }

    #[test]
    fn bandwidth_window_rejects_short_and_sparse_transfers() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        for offset_ms in [0, 10, 20, 30] {
            window.record(start + Duration::from_millis(offset_ms), 128, 128);
        }
        assert_eq!(window.estimate(start + BANDWIDTH_BUCKET, true), 0);
        assert_eq!(window.estimate(start + BANDWIDTH_BUCKET, false), 0);
    }

    #[test]
    fn bandwidth_window_reports_recent_sparse_business_average() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        window.record(start, 1_024, 2_048);
        assert_eq!(
            window.estimate(start + Duration::from_secs(1), true),
            40_960
        );
        assert_eq!(
            window.estimate(start + Duration::from_secs(1), false),
            81_920
        );
        assert_eq!(
            window.estimate(start + Duration::from_secs(30), true),
            40_960
        );
        assert_eq!(window.estimate(start + BANDWIDTH_ESTIMATE_TTL, true), 0);
    }

    #[test]
    fn bandwidth_window_retains_samples_before_first_query() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        record_transfer(&mut window, start, 2_000, 0);
        window.record(start, 0, 1_024);

        assert_eq!(
            window.estimate(start + Duration::from_secs(30), true),
            320_000
        );
        assert_eq!(
            window.estimate(start + Duration::from_secs(30), false),
            40_960
        );
    }

    #[test]
    fn bandwidth_window_updates_sparse_samples_and_their_expiry() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        window.record(start, 1_024, 0);
        assert_eq!(
            window.estimate(start + Duration::from_secs(1), true),
            40_960
        );

        let latest = start + Duration::from_secs(60);
        window.record(latest, 2_048, 0);
        assert_eq!(
            window.estimate(latest + Duration::from_secs(1), true),
            81_920
        );
        assert_eq!(
            window.estimate(start + BANDWIDTH_ESTIMATE_TTL, true),
            81_920
        );
        assert_eq!(window.estimate(latest + BANDWIDTH_ESTIMATE_TTL, true), 0);
    }

    #[test]
    fn bandwidth_window_aggregates_sparse_payloads_across_buckets() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        window.record(start, 512, 0);
        window.record(start + Duration::from_secs(1), 512, 0);
        assert_eq!(window.estimate(start + Duration::from_secs(1), true), 8_192);

        window.record(start + Duration::from_secs(2), 512, 0);
        assert_eq!(
            window.estimate(start + Duration::from_secs(30), true),
            6_144
        );
        assert_eq!(window.estimate(start + Duration::from_secs(30), false), 0);
    }

    #[test]
    fn bandwidth_window_other_direction_does_not_extend_estimate() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        window.record(start, 1_024, 0);
        window.record(start + Duration::from_secs(290), 0, 1_024);
        assert_eq!(window.estimate(start + BANDWIDTH_ESTIMATE_TTL, true), 0);
        assert_eq!(
            window.estimate(start + BANDWIDTH_ESTIMATE_TTL, false),
            40_960
        );
    }

    #[test]
    fn bandwidth_window_keeps_later_sparse_sample_after_peak_expires() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        record_transfer(&mut window, start, 2_000, 0);
        let latest = start + Duration::from_secs(290);
        window.record(latest, 1_024, 0);

        assert_eq!(
            window.estimate(start + BANDWIDTH_ESTIMATE_TTL, true),
            320_000
        );
        assert_eq!(
            window.estimate(start + BANDWIDTH_ESTIMATE_TTL + BANDWIDTH_BUCKET, true),
            40_960
        );
        assert_eq!(window.estimate(latest + BANDWIDTH_ESTIMATE_TTL, true), 0);
    }

    #[test]
    fn total_traffic_counters_do_not_contaminate_bandwidth_samples() {
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
    fn bandwidth_filter_excludes_ping_and_rpc_but_samples_business_data() {
        use crate::{
            packet::{PacketType, ZCPacket},
            tunnel::filter::{
                BandwidthRecorderTunnelFilter, StatsRecorderTunnelFilter, TunnelFilter,
                TunnelFilterChain,
            },
        };

        let stats_filter = StatsRecorderTunnelFilter::new();
        let throughput = stats_filter.get_throughput();
        let filter = TunnelFilterChain::new(
            BandwidthRecorderTunnelFilter::new(throughput.clone()),
            stats_filter,
        );
        for packet_type in [
            PacketType::Ping,
            PacketType::Pong,
            PacketType::RpcReq,
            PacketType::RpcResp,
        ] {
            let mut packet = ZCPacket::new_with_payload(&[0; 5_000]);
            packet.fill_peer_manager_hdr(1, 2, packet_type as u8);
            filter.before_send(packet.clone()).unwrap();
            filter.after_received(Ok(packet)).unwrap().unwrap();
        }
        assert_eq!(sampled_data_bytes(&throughput), (0, 0));
        assert_eq!(throughput.tx_packets(), 4);
        assert_eq!(throughput.rx_packets(), 4);

        let mut packet = ZCPacket::new_with_payload(&[0; 5_000]);
        packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        filter.before_send(packet.clone()).unwrap();
        filter.after_received(Ok(packet)).unwrap().unwrap();
        assert_eq!(sampled_data_bytes(&throughput), (5_000, 5_000));
    }

    #[test]
    fn bandwidth_filter_skips_unclassified_foreign_packets() {
        use crate::{
            packet::{PacketType, ZCPacket},
            tunnel::filter::{BandwidthRecorderTunnelFilter, TunnelFilter},
        };

        let throughput = Arc::new(Throughput::new());
        let filter = BandwidthRecorderTunnelFilter::new(throughput.clone());
        let mut inner = ZCPacket::new_with_payload(&[0; 5_000]);
        inner.fill_peer_manager_hdr(1, 2, PacketType::RpcReq as u8);
        let control = ZCPacket::new_for_foreign_network(&"other".to_owned(), 2, &inner);
        filter.before_send(control.clone()).unwrap();
        filter.after_received(Ok(control)).unwrap().unwrap();

        inner.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        let mut encrypted = ZCPacket::new_for_foreign_network(&"other".to_owned(), 2, &inner);
        encrypted
            .mut_peer_manager_header()
            .unwrap()
            .set_encrypted(true);
        filter.before_send(encrypted.clone()).unwrap();
        filter.after_received(Ok(encrypted)).unwrap().unwrap();
        assert_eq!(sampled_data_bytes(&throughput), (0, 0));

        let business = ZCPacket::new_for_foreign_network(&"other".to_owned(), 2, &inner);
        filter.before_send(business.clone()).unwrap();
        filter.after_received(Ok(business)).unwrap().unwrap();
        assert_eq!(sampled_data_bytes(&throughput), (5_000, 5_000));
    }

    #[test]
    fn bandwidth_filter_inside_authentication_does_not_sample_rejected_packets() {
        use crate::{
            packet::{PacketType, ZCPacket},
            tunnel::{
                SinkItem, StreamItem,
                filter::{
                    BandwidthRecorderTunnelFilter, StatsRecorderTunnelFilter, TunnelFilter,
                    TunnelFilterChain,
                },
            },
        };

        struct AuthenticationFilter;
        impl TunnelFilter for AuthenticationFilter {
            type FilterOutput = ();

            fn before_send(&self, mut packet: SinkItem) -> Option<SinkItem> {
                packet
                    .mut_peer_manager_header()
                    .unwrap()
                    .set_encrypted(true);
                Some(packet)
            }

            fn after_received(&self, packet: StreamItem) -> Option<StreamItem> {
                let packet = packet.ok()?;
                (!packet.peer_manager_header()?.is_encrypted()).then_some(Ok(packet))
            }

            fn filter_output(&self) {}
        }

        let stats_filter = StatsRecorderTunnelFilter::new();
        let throughput = stats_filter.get_throughput();
        let filter = TunnelFilterChain::new(
            BandwidthRecorderTunnelFilter::new(throughput.clone()),
            AuthenticationFilter,
        )
        .chain(stats_filter);
        let mut packet = ZCPacket::new_with_payload(&[0; 5_000]);
        packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        let encrypted = filter.before_send(packet.clone()).unwrap();
        assert!(encrypted.peer_manager_header().unwrap().is_encrypted());
        assert!(filter.after_received(Ok(encrypted)).is_none());
        assert_eq!(sampled_data_bytes(&throughput), (5_000, 0));
        assert_eq!(throughput.rx_packets(), 1);

        filter.after_received(Ok(packet)).unwrap().unwrap();
        assert_eq!(sampled_data_bytes(&throughput), (5_000, 5_000));
    }
}
