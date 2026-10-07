use std::{fmt::Debug, time::Duration};

pub const BANDWIDTH_ESTIMATE_VERSION: u32 = 1;

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
        let rate = u128::from(bytes) * 8 * 1_000_000_000 / self.rtt.as_nanos();
        Some(rate.min(u128::from(u64::MAX)) as u64).filter(|rate| *rate > 0)
    }
}

pub trait TransmissionWindowSource: Debug + Send + Sync {
    fn transmission_window(&self) -> Option<TransmissionWindow>;
}

const REPORT_MAGIC: &[u8; 4] = b"ETBW";
pub(crate) const WINDOW_REPORT_LEN: usize = 20;

pub(crate) fn encode_window_report(seq: u32, bps: u64, reply: bool) -> [u8; WINDOW_REPORT_LEN] {
    let mut payload = [0; WINDOW_REPORT_LEN];
    payload[..4].copy_from_slice(&seq.to_le_bytes());
    payload[4..8].copy_from_slice(REPORT_MAGIC);
    payload[8] = 1;
    payload[9] = u8::from(reply);
    payload[12..].copy_from_slice(&bps.to_le_bytes());
    payload
}

pub(crate) fn decode_window_report(payload: &[u8], reply: bool) -> Option<u64> {
    if payload.len() != WINDOW_REPORT_LEN
        || &payload[4..8] != REPORT_MAGIC
        || payload[8] != 1
        || payload[9] != u8::from(reply)
        || payload[10..12] != [0, 0]
    {
        return None;
    }
    Some(u64::from_le_bytes(payload[12..].try_into().ok()?))
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
    fn preserves_submillisecond_rtt_and_saturates_without_overflow() {
        let window = TransmissionWindow {
            congestion_window_bytes: 125_000,
            peer_receive_window_bytes: None,
            rtt: Duration::from_micros(500),
        };
        assert_eq!(window.estimated_bps(), Some(2_000_000_000));
        assert_eq!(
            TransmissionWindow {
                congestion_window_bytes: u64::MAX,
                rtt: Duration::from_nanos(1),
                ..window
            }
            .estimated_bps(),
            Some(u64::MAX),
        );
    }

    #[test]
    fn distinguishes_a_remote_reply_from_an_old_peers_echoed_request() {
        let request = encode_window_report(7, 40_000_000, false);
        let reply = encode_window_report(7, 160_000_000, true);
        assert_eq!(&request[..4], &7_u32.to_le_bytes());
        assert_eq!(decode_window_report(&request, false), Some(40_000_000));
        assert_eq!(decode_window_report(&request, true), None);
        assert_eq!(decode_window_report(&reply, true), Some(160_000_000));
    }

    #[test]
    fn safely_ignores_legacy_truncated_and_unknown_version_reports() {
        let mut payload = encode_window_report(1, 100, true);
        for len in 0..WINDOW_REPORT_LEN {
            assert_eq!(decode_window_report(&payload[..len], true), None);
        }
        payload[8] = 2;
        assert_eq!(decode_window_report(&payload, true), None);
    }
}
