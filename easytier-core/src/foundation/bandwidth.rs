use std::{fmt::Debug, time::Duration};

/// A sampled bidirectional traffic report exchanged with the peer.
///
/// The rates are tunnel-traffic observations, not a speed-test result.
/// `estimated_tx_bps` is the sender's own outbound estimate, while `tx_bps`
/// and `rx_bps` describe the traffic observed during `sample_window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrafficRateReport {
    pub estimated_tx_bps: u64,
    pub tx_bps: u64,
    pub rx_bps: u64,
    pub sample_window: Duration,
}

/// A simple steady-state correction for congestion-control window headroom.
/// BBR commonly targets about two BDPs of window; startup, recovery and ACK
/// aggregation can still make this approximation differ from its bandwidth model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CongestionControl {
    /// CUBIC, New Reno and friends: the window tracks roughly one BDP.
    #[default]
    LossBased,
    /// Approximate BBR's usual window gain of two, paired with min RTT.
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
    /// loss-based control. An application-level Ping RTT includes scheduling
    /// and tunnel processing and must not replace this transport measurement.
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
        let rate = u128::from(bytes) * 8 * 1_000_000_000 / self.rtt.as_nanos();
        u64::try_from(rate).ok().filter(|rate| *rate > 0)
    }
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
    fn preserves_sub_millisecond_transport_rtt_and_rejects_overflow() {
        let window = loss_based(125_000, None, Duration::from_micros(500));
        assert_eq!(window.estimated_bps(), Some(2_000_000_000));
        assert_eq!(
            loss_based(25_000, Some(64_000), Duration::from_micros(200)).estimated_bps(),
            Some(1_000_000_000),
        );
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
    fn bbr_steady_state_windows_are_corrected_for_headroom() {
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
}
