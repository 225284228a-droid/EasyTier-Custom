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
    /// Sparse or very short transfers provide insufficient evidence and
    /// return zero; it does not measure unused headroom.
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
        }
    }

    fn sequence(&self, now: Instant) -> u64 {
        (now.saturating_duration_since(self.started_at).as_millis() / BANDWIDTH_BUCKET.as_millis())
            as u64
    }

    fn record(&mut self, now: Instant, tx_bytes: u64, rx_bytes: u64) {
        let sequence = self.sequence(now);
        let bucket = &mut self.buckets[sequence as usize % BANDWIDTH_BUCKET_COUNT];
        if bucket.sequence != Some(sequence) {
            *bucket = BandwidthBucket {
                sequence: Some(sequence),
                ..Default::default()
            };
        }
        bucket.tx.record(now, tx_bytes);
        bucket.rx.record(now, rx_bytes);
    }

    fn estimate(&self, now: Instant, tx: bool) -> u64 {
        let sequence = self.sequence(now);
        self.buckets
            .iter()
            .filter(|bucket| {
                bucket.sequence.is_some_and(|recorded| {
                    recorded <= sequence && sequence - recorded < BANDWIDTH_BUCKET_COUNT as u64
                })
            })
            .map(|bucket| if tx { &bucket.tx } else { &bucket.rx })
            .map(DirectionSample::estimate)
            .max()
            .unwrap_or_default()
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
        assert_eq!(window.estimate(start + BANDWIDTH_WINDOW, true), 0);
        assert_eq!(window.estimate(start + BANDWIDTH_WINDOW, false), 0);
    }

    #[test]
    fn bandwidth_window_requires_sustained_samples_in_each_direction() {
        let start = Instant::now();
        let mut window = BandwidthWindow::new(start);
        record_transfer(&mut window, start, 2_000, 0);
        window.record(start, 0, 100_000);
        assert_eq!(window.estimate(start + BANDWIDTH_BUCKET, true), 320_000);
        assert_eq!(window.estimate(start + BANDWIDTH_BUCKET, false), 0);

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
            window.record(start + Duration::from_millis(offset_ms), 50_000, 500);
        }
        assert_eq!(window.estimate(start + BANDWIDTH_BUCKET, true), 0);
        assert_eq!(window.estimate(start + BANDWIDTH_BUCKET, false), 0);
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
