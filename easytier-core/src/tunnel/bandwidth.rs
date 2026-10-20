use std::time::Duration;

pub use crate::foundation::bandwidth::{
    TrafficRateReport, TransmissionWindow, TransmissionWindowSource,
};

pub const BANDWIDTH_ESTIMATE_VERSION: u32 = 2;

const LEGACY_REPORT_VERSION: u32 = 1;
const REPORT_MAGIC: &[u8; 4] = b"ETBW";
const LEGACY_WINDOW_REPORT_LEN: usize = 20;
pub(crate) const WINDOW_REPORT_LEN: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowReport {
    pub version: u32,
    pub report: TrafficRateReport,
}

pub(crate) fn encode_window_report(
    seq: u32,
    report: TrafficRateReport,
    reply: bool,
    version: u32,
) -> Vec<u8> {
    let len = match version {
        LEGACY_REPORT_VERSION => LEGACY_WINDOW_REPORT_LEN,
        BANDWIDTH_ESTIMATE_VERSION => WINDOW_REPORT_LEN,
        _ => return Vec::new(),
    };
    let mut payload = vec![0; len];
    payload[..4].copy_from_slice(&seq.to_le_bytes());
    payload[4..8].copy_from_slice(REPORT_MAGIC);
    payload[8] = version as u8;
    payload[9] = u8::from(reply);
    payload[12..20].copy_from_slice(&report.estimated_tx_bps.to_le_bytes());
    if version == BANDWIDTH_ESTIMATE_VERSION {
        payload[20..28].copy_from_slice(&report.tx_bps.to_le_bytes());
        payload[28..36].copy_from_slice(&report.rx_bps.to_le_bytes());
        let sample_window_us = u32::try_from(report.sample_window.as_micros()).unwrap_or(u32::MAX);
        payload[36..40].copy_from_slice(&sample_window_us.to_le_bytes());
    }
    payload
}

pub(crate) fn decode_window_report(payload: &[u8], reply: bool) -> Option<WindowReport> {
    if payload.len() < 12
        || &payload[4..8] != REPORT_MAGIC
        || payload[9] != u8::from(reply)
        || payload[10..12] != [0, 0]
    {
        return None;
    }

    let version = u32::from(payload[8]);
    match version {
        LEGACY_REPORT_VERSION if payload.len() == LEGACY_WINDOW_REPORT_LEN => Some(WindowReport {
            version,
            report: TrafficRateReport {
                estimated_tx_bps: u64::from_le_bytes(payload[12..20].try_into().ok()?),
                ..Default::default()
            },
        }),
        BANDWIDTH_ESTIMATE_VERSION if payload.len() == WINDOW_REPORT_LEN => Some(WindowReport {
            version,
            report: TrafficRateReport {
                estimated_tx_bps: u64::from_le_bytes(payload[12..20].try_into().ok()?),
                tx_bps: u64::from_le_bytes(payload[20..28].try_into().ok()?),
                rx_bps: u64::from_le_bytes(payload[28..36].try_into().ok()?),
                sample_window: Duration::from_micros(u64::from(u32::from_le_bytes(
                    payload[36..40].try_into().ok()?,
                ))),
            },
        }),
        _ => None,
    }
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
        let request = encode_window_report(7, report(), false, BANDWIDTH_ESTIMATE_VERSION);
        let reply = encode_window_report(7, report(), true, BANDWIDTH_ESTIMATE_VERSION);
        assert_eq!(request.len(), WINDOW_REPORT_LEN);
        assert_eq!(&request[..4], &7_u32.to_le_bytes());
        assert_eq!(
            decode_window_report(&request, false),
            Some(WindowReport {
                version: BANDWIDTH_ESTIMATE_VERSION,
                report: report(),
            })
        );
        assert_eq!(decode_window_report(&request, true), None);
        assert_eq!(
            decode_window_report(&reply, true)
                .expect("valid reply")
                .report,
            report()
        );
    }

    #[test]
    fn still_accepts_and_replies_to_v1_peers() {
        let legacy = TrafficRateReport {
            estimated_tx_bps: 40_000_000,
            ..Default::default()
        };
        let request = encode_window_report(9, legacy, false, LEGACY_REPORT_VERSION);
        assert_eq!(request.len(), LEGACY_WINDOW_REPORT_LEN);
        assert_eq!(
            decode_window_report(&request, false).expect("legacy request"),
            WindowReport {
                version: LEGACY_REPORT_VERSION,
                report: legacy,
            }
        );

        let reply = encode_window_report(9, legacy, true, LEGACY_REPORT_VERSION);
        assert_eq!(
            decode_window_report(&reply, true).expect("legacy reply"),
            WindowReport {
                version: LEGACY_REPORT_VERSION,
                report: legacy,
            }
        );
    }

    #[test]
    fn safely_ignores_truncated_and_unknown_version_reports() {
        let payload = encode_window_report(1, report(), true, BANDWIDTH_ESTIMATE_VERSION);
        for len in 0..WINDOW_REPORT_LEN {
            assert_eq!(decode_window_report(&payload[..len], true), None);
        }

        let mut payload = payload;
        payload[8] = 3;
        assert_eq!(decode_window_report(&payload, true), None);
    }
}
