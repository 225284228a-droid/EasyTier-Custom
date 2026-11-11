//! Passive bandwidth estimation for [`Throughput`].
//!
//! Transport windows and delivery reports from the peer feed the filters
//! below; nothing here sends extra traffic.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::foundation::bandwidth::{TrafficRateReport, TransmissionWindowSource};

use super::Throughput;

pub(super) const PEER_WINDOW_REPORT_TTL: Duration = Duration::from_secs(90);
const RATE_SAMPLE_MIN_INTERVAL: Duration = Duration::from_millis(500);
const RATE_SAMPLE_MAX_INTERVAL: Duration = Duration::from_secs(5);
const RATE_SAMPLE_TTL: Duration = Duration::from_secs(5);
const RATE_SAMPLE_MIN_BYTES: u64 = 4 * 1024;

pub(super) const DELIVERY_SAMPLE_TTL: Duration = Duration::from_secs(10);
const DELIVERY_SAMPLE_LIMIT: usize = 20;
pub(super) const DELIVERY_MIN_BPS: u64 = 64_000;

#[derive(Debug, Clone, Copy)]
pub(super) struct RateSnapshot {
    pub(super) observed_at: Instant,
    pub(super) report: TrafficRateReport,
}

#[derive(Debug)]
pub(super) struct RateState {
    pub(super) baseline_at: Instant,
    pub(super) tx_bytes: u64,
    pub(super) rx_bytes: u64,
    pub(super) last: Option<RateSnapshot>,
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
pub(super) struct DeliveryRate {
    samples: VecDeque<(Instant, u64)>,
}

impl DeliveryRate {
    pub(super) fn record(&mut self, delivered_bps: u64, sample_window: Duration, now: Instant) {
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

    pub(super) fn estimate(&mut self, now: Instant) -> u64 {
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
pub(super) struct DeliveryState {
    pub(super) tx: DeliveryRate,
    pub(super) rx: DeliveryRate,
}

impl Throughput {
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
        peer_rx_bps: u64,
        peer_estimated_tx_bps: u64,
        peer_sample_window: Duration,
    ) {
        self.record_peer_window_estimate(peer_estimated_tx_bps);
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

    pub(super) fn estimated_tx_bps_at(&self, now: Instant) -> u64 {
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

    pub(super) fn peer_estimate_at(&self, now: Instant) -> u64 {
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
