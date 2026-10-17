use std::{
    collections::VecDeque,
    fmt::Debug,
    time::{Duration, Instant},
};

// Very short transport RTTs can measure a local proxy or timer noise instead of
// the tunnel path. This makes the window estimate conservative on fast LANs.
const MIN_ESTIMATE_RTT: Duration = Duration::from_millis(1);

/// Congestion-control families need different math before a congestion window
/// reads as a bottleneck rate. Loss-based algorithms (CUBIC, New Reno, CTCP)
/// settle near one BDP plus the standing queue, while BBR deliberately keeps
/// about two BDPs of window (cwnd gain 2 in both quinn's BBR and Linux's
/// `bbr_cwnd_gain`), so an uncorrected BBR window doubles the estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CongestionControl {
    /// CUBIC, New Reno and friends: the window tracks roughly one BDP.
    #[default]
    LossBased,
    /// BBR keeps roughly two BDPs in flight; pair it with the RTprop (min
    /// RTT) denominator BBR itself uses for its BDP estimate.
    Bbr,
}

impl CongestionControl {
    fn window_gain(self) -> u64 {
        match self {
            Self::LossBased => 1,
            Self::Bbr => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransmissionWindow {
    pub congestion_window_bytes: u64,
    pub peer_receive_window_bytes: Option<u64>,
    pub rtt: Duration,
    pub congestion_control: CongestionControl,
}

impl TransmissionWindow {
    /// Window-limited throughput, not unused link capacity or a speed test.
    /// The window must be paired with the RTT of the same scope: the source
    /// picks the transport's min RTT for BBR and its smoothed RTT for
    /// loss-based control, and the caller raises it to the tunnel's
    /// full round trip when a relay or proxy extends the path.
    pub fn estimated_bps(self) -> Option<u64> {
        let congestion_window_bytes =
            self.congestion_window_bytes / self.congestion_control.window_gain();
        let bytes = self
            .peer_receive_window_bytes
            .map_or(congestion_window_bytes, |receive| {
                receive.min(congestion_window_bytes)
            });
        if bytes == 0 || self.rtt.is_zero() {
            return None;
        }
        let rtt = self.rtt.max(MIN_ESTIMATE_RTT);
        let rate = u128::from(bytes) * 8 * 1_000_000_000 / rtt.as_nanos();
        u64::try_from(rate).ok().filter(|rate| *rate > 0)
    }
}

/// A time-boxed median over recently observed transport windows. Individual
/// passive samples swing for reasons that carry no capacity information: BBR
/// collapses its window to the minimum for one ProbeRTT interval (about
/// 200 ms out of every RTprop window), and loss-based windows halve on a
/// single loss event before regrowing. Reporting the median across a few
/// seconds of samples keeps the estimate at the path's steady window instead
/// of one unlucky poll.
#[derive(Debug)]
pub struct RecentWindows {
    span: Duration,
    samples: VecDeque<(Instant, TransmissionWindow)>,
}

impl RecentWindows {
    pub fn new(span: Duration) -> Self {
        Self {
            span,
            samples: VecDeque::new(),
        }
    }

    /// Retains only samples inside the span; callers filter invalid samples
    /// before recording, matching each source's own validity rules.
    pub fn record(&mut self, now: Instant, window: TransmissionWindow) {
        self.samples.push_back((now, window));
        while let Some((observed_at, _)) = self.samples.front()
            && now.saturating_duration_since(*observed_at) > self.span
        {
            self.samples.pop_front();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The median window across the retained samples. Window bytes and RTT
    /// are medians of their own series; window kind and receive-window
    /// presence come from the newest sample because a socket's congestion
    /// control never changes mid-connection.
    pub fn median(&self) -> Option<TransmissionWindow> {
        let newest = self.samples.back()?.1;
        let congestion_windows: Vec<u64> = self
            .samples
            .iter()
            .map(|(_, window)| window.congestion_window_bytes)
            .collect();
        let rtts: Vec<u128> = self
            .samples
            .iter()
            .map(|(_, window)| window.rtt.as_nanos())
            .collect();
        let peer_receive_window_bytes = newest.peer_receive_window_bytes.map(|newest_receive| {
            let receives: Vec<u64> = self
                .samples
                .iter()
                .filter_map(|(_, window)| window.peer_receive_window_bytes)
                .collect();
            median_u64(&receives).unwrap_or(newest_receive)
        });
        Some(TransmissionWindow {
            congestion_window_bytes: median_u64(&congestion_windows)?,
            peer_receive_window_bytes,
            rtt: Duration::from_nanos(u64::try_from(median_u128(&rtts)?).ok()?),
            congestion_control: newest.congestion_control,
        })
    }
}

fn median_u64(values: &[u64]) -> Option<u64> {
    let mut values = values.to_vec();
    values.sort_unstable();
    let mid = values.len() / 2;
    values.get(mid).copied().map(|upper| {
        // Even counts average the two central samples; odd counts take the
        // middle. The sum of a pair of window byte counts cannot overflow.
        if values.len().is_multiple_of(2) {
            let lower = values[mid - 1];
            lower / 2 + upper / 2 + u64::from(lower % 2 == 1 && upper % 2 == 1)
        } else {
            upper
        }
    })
}

fn median_u128(values: &[u128]) -> Option<u128> {
    let mut values = values.to_vec();
    values.sort_unstable();
    let mid = values.len() / 2;
    values.get(mid).copied().map(|upper| {
        if values.len().is_multiple_of(2) {
            let lower = values[mid - 1];
            lower / 2 + upper / 2 + u128::from(lower % 2 == 1 && upper % 2 == 1)
        } else {
            upper
        }
    })
}

pub trait TransmissionWindowSource: Debug + Send + Sync {
    fn transmission_window(&self) -> Option<TransmissionWindow>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loss_based(
        congestion_window_bytes: u64,
        peer_receive_window_bytes: Option<u64>,
        rtt: Duration,
    ) -> TransmissionWindow {
        TransmissionWindow {
            congestion_window_bytes,
            peer_receive_window_bytes,
            rtt,
            congestion_control: CongestionControl::LossBased,
        }
    }

    #[test]
    fn uses_bytes_bits_and_seconds_without_a_traffic_sample() {
        let window = loss_based(1_000_000, None, Duration::from_millis(100));
        assert_eq!(window.estimated_bps(), Some(80_000_000));
    }

    #[test]
    fn limits_tcp_to_the_smaller_actual_window() {
        let window = loss_based(1_000_000, Some(250_000), Duration::from_millis(50));
        assert_eq!(window.estimated_bps(), Some(40_000_000));
        assert_eq!(
            TransmissionWindow {
                peer_receive_window_bytes: Some(2_000_000),
                ..window
            }
            .estimated_bps(),
            Some(160_000_000),
        );
    }

    #[test]
    fn rejects_missing_rtt_or_a_closed_window() {
        let window = loss_based(12_000, None, Duration::ZERO);
        assert_eq!(window.estimated_bps(), None);
        assert_eq!(
            TransmissionWindow {
                congestion_window_bytes: 0,
                rtt: Duration::from_millis(1),
                ..window
            }
            .estimated_bps(),
            None,
        );
        assert_eq!(
            TransmissionWindow {
                peer_receive_window_bytes: Some(0),
                rtt: Duration::from_millis(1),
                ..window
            }
            .estimated_bps(),
            None,
        );
    }

    #[test]
    fn conservatively_bounds_near_zero_rtt_and_rejects_overflow() {
        let window = loss_based(125_000, None, Duration::from_micros(500));
        assert_eq!(window.estimated_bps(), Some(1_000_000_000));
        assert_eq!(
            TransmissionWindow {
                congestion_window_bytes: u64::MAX,
                rtt: Duration::from_nanos(1),
                ..window
            }
            .estimated_bps(),
            None,
        );
    }

    #[test]
    fn does_not_treat_ten_gigabits_as_a_physical_limit() {
        assert_eq!(
            loss_based(2_500_000, None, Duration::from_millis(1)).estimated_bps(),
            Some(20_000_000_000),
        );
    }

    #[test]
    fn bbr_windows_report_the_bottleneck_rate_not_double() {
        // A BBR sender that measured a 1 Gbps / 40 ms path keeps roughly two
        // BDPs (10 MB) of window. The uncorrected division would claim
        // 2 Gbps; the gain correction restores the measured rate.
        let window = TransmissionWindow {
            congestion_window_bytes: 10_000_000,
            peer_receive_window_bytes: None,
            rtt: Duration::from_millis(40),
            congestion_control: CongestionControl::Bbr,
        };
        assert_eq!(window.estimated_bps(), Some(1_000_000_000));
    }

    #[test]
    fn bbr_correction_still_applies_the_peer_receive_window() {
        let window = TransmissionWindow {
            congestion_window_bytes: 4_000_000,
            peer_receive_window_bytes: Some(1_000_000),
            rtt: Duration::from_millis(10),
            congestion_control: CongestionControl::Bbr,
        };
        // min(4 MB / 2, 1 MB) = 1 MB over 10 ms.
        assert_eq!(window.estimated_bps(), Some(800_000_000));
    }

    #[test]
    fn a_single_probe_rtt_dip_does_not_drag_the_reported_window_down() {
        let span = Duration::from_secs(10);
        let mut recent = RecentWindows::new(span);
        let now = Instant::now();
        let steady = loss_based(1_000_000, Some(500_000), Duration::from_millis(20));
        for second in 0..5 {
            recent.record(now + Duration::from_secs(second), steady);
        }
        // BBR ProbeRTT collapses the window for a fraction of a second.
        recent.record(
            now + Duration::from_secs(5),
            TransmissionWindow {
                congestion_window_bytes: 6_000,
                peer_receive_window_bytes: Some(3_000),
                rtt: Duration::from_millis(4),
                ..steady
            },
        );
        let median = recent.median().unwrap();
        assert_eq!(median.congestion_window_bytes, 1_000_000);
        assert_eq!(median.peer_receive_window_bytes, Some(500_000));
        assert_eq!(median.rtt, Duration::from_millis(20));
        assert_eq!(median.estimated_bps(), Some(200_000_000));
    }

    #[test]
    fn samples_expire_with_the_span_and_kind_follows_the_newest_sample() {
        let span = Duration::from_secs(10);
        let mut recent = RecentWindows::new(span);
        let now = Instant::now();
        recent.record(now, loss_based(1_000_000, None, Duration::from_millis(10)));
        assert_eq!(recent.median().unwrap().congestion_window_bytes, 1_000_000);

        recent.record(
            now + span + Duration::from_secs(1),
            TransmissionWindow {
                congestion_window_bytes: 2_000_000,
                peer_receive_window_bytes: None,
                rtt: Duration::from_millis(10),
                congestion_control: CongestionControl::Bbr,
            },
        );
        let median = recent.median().unwrap();
        assert_eq!(median.congestion_window_bytes, 2_000_000);
        assert_eq!(median.congestion_control, CongestionControl::Bbr);
        assert_eq!(median.estimated_bps(), Some(800_000_000));
    }

    #[test]
    fn an_empty_history_has_no_median() {
        assert!(
            RecentWindows::new(Duration::from_secs(10))
                .median()
                .is_none()
        );
        assert!(RecentWindows::new(Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn even_sample_counts_average_the_two_central_values() {
        let mut recent = RecentWindows::new(Duration::from_secs(10));
        let now = Instant::now();
        for (window_bytes, rtt_ms) in [(600_000, 10), (1_000_000, 20)] {
            recent.record(
                now,
                loss_based(window_bytes, None, Duration::from_millis(rtt_ms)),
            );
        }
        let median = recent.median().unwrap();
        assert_eq!(median.congestion_window_bytes, 800_000);
        assert_eq!(median.rtt, Duration::from_millis(15));
    }
}
