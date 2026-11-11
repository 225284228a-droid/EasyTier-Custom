use std::time::Duration;

pub use crate::foundation::bandwidth::{
    TrafficRateReport, TransmissionWindow, TransmissionWindowSource,
};

pub const BANDWIDTH_ESTIMATE_VERSION: u32 = 2;

const REPORT_MAGIC: &[u8; 4] = b"ETBW";
pub(crate) const WINDOW_REPORT_LEN: usize = 40;

pub(crate) fn encode_window_report(seq: u32, report: TrafficRateReport, reply: bool) -> Vec<u8> {
    let mut payload = vec![0; WINDOW_REPORT_LEN];
    payload[..4].copy_from_slice(&seq.to_le_bytes());
    payload[4..8].copy_from_slice(REPORT_MAGIC);
    payload[8] = BANDWIDTH_ESTIMATE_VERSION as u8;
    payload[9] = u8::from(reply);
    payload[12..20].copy_from_slice(&report.estimated_tx_bps.to_le_bytes());
    payload[20..28].copy_from_slice(&report.tx_bps.to_le_bytes());
    payload[28..36].copy_from_slice(&report.rx_bps.to_le_bytes());
    let sample_window_us = u32::try_from(report.sample_window.as_micros()).unwrap_or(u32::MAX);
    payload[36..40].copy_from_slice(&sample_window_us.to_le_bytes());
    payload
}

pub(crate) fn decode_window_report(payload: &[u8], reply: bool) -> Option<TrafficRateReport> {
    if payload.len() != WINDOW_REPORT_LEN
        || &payload[4..8] != REPORT_MAGIC
        || u32::from(payload[8]) != BANDWIDTH_ESTIMATE_VERSION
        || payload[9] != u8::from(reply)
        || payload[10..12] != [0, 0]
    {
        return None;
    }
    Some(TrafficRateReport {
        estimated_tx_bps: u64::from_le_bytes(payload[12..20].try_into().ok()?),
        tx_bps: u64::from_le_bytes(payload[20..28].try_into().ok()?),
        rx_bps: u64::from_le_bytes(payload[28..36].try_into().ok()?),
        sample_window: Duration::from_micros(u64::from(u32::from_le_bytes(
            payload[36..40].try_into().ok()?,
        ))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> TrafficRateReport {
        TrafficRateReport {
            estimated_tx_bps: 80_000_000,
            tx_bps: 100_000_000,
            rx_bps: 20_000_000,
            sample_window: Duration::from_secs(1),
        }
    }

    #[test]
    fn round_trips_v2_traffic_reports() {
        let request = encode_window_report(7, report(), false);
        let reply = encode_window_report(7, report(), true);
        assert_eq!(request.len(), WINDOW_REPORT_LEN);
        assert_eq!(&request[..4], &7_u32.to_le_bytes());
        assert_eq!(decode_window_report(&request, false), Some(report()));
        assert_eq!(decode_window_report(&request, true), None);
        assert_eq!(decode_window_report(&reply, true), Some(report()));
    }

    #[test]
    fn safely_ignores_truncated_and_unknown_version_reports() {
        let payload = encode_window_report(1, report(), true);
        for len in 0..WINDOW_REPORT_LEN {
            assert_eq!(decode_window_report(&payload[..len], true), None);
        }

        let mut payload = payload;
        for version in [1, 3] {
            payload[8] = version;
            assert_eq!(decode_window_report(&payload, true), None);
        }
    }
}
