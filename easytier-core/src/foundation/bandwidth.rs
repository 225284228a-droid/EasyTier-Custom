use std::{fmt::Debug, time::Duration};

// Very short transport RTTs can measure a local proxy or timer noise instead of
// the tunnel path. This makes the window estimate conservative on fast LANs.
const MIN_ESTIMATE_RTT: Duration = Duration::from_millis(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransmissionWindow {
    pub congestion_window_bytes: u64,
    pub peer_receive_window_bytes: Option<u64>,
    pub rtt: Duration,
}

impl TransmissionWindow {
    /// Window-limited throughput, not unused link capacity or a speed test.
    pub fn estimated_bps(self) -> Option<u64> {
        let bytes = self
            .peer_receive_window_bytes
            .map_or(self.congestion_window_bytes, |receive| {
                receive.min(self.congestion_window_bytes)
            });
        if bytes == 0 || self.rtt.is_zero() {
            return None;
        }
        let rtt = self.rtt.max(MIN_ESTIMATE_RTT);
        let rate = u128::from(bytes) * 8 * 1_000_000_000 / rtt.as_nanos();
        u64::try_from(rate).ok().filter(|rate| *rate > 0)
    }
}

pub trait TransmissionWindowSource: Debug + Send + Sync {
    fn transmission_window(&self) -> Option<TransmissionWindow>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_bytes_bits_and_seconds_without_a_traffic_sample() {
        let window = TransmissionWindow {
            congestion_window_bytes: 1_000_000,
            peer_receive_window_bytes: None,
            rtt: Duration::from_millis(100),
        };
        assert_eq!(window.estimated_bps(), Some(80_000_000));
    }

    #[test]
    fn limits_tcp_to_the_smaller_actual_window() {
        let window = TransmissionWindow {
            congestion_window_bytes: 1_000_000,
            peer_receive_window_bytes: Some(250_000),
            rtt: Duration::from_millis(50),
        };
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
        let window = TransmissionWindow {
            congestion_window_bytes: 12_000,
            peer_receive_window_bytes: None,
            rtt: Duration::ZERO,
        };
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
        let window = TransmissionWindow {
            congestion_window_bytes: 125_000,
            peer_receive_window_bytes: None,
            rtt: Duration::from_micros(500),
        };
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
            TransmissionWindow {
                congestion_window_bytes: 2_500_000,
                peer_receive_window_bytes: None,
                rtt: Duration::from_millis(1),
            }
            .estimated_bps(),
            Some(20_000_000_000),
        );
    }
}
