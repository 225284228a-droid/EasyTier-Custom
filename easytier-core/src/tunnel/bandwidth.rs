pub use crate::foundation::bandwidth::{TransmissionWindow, TransmissionWindowSource};

pub const BANDWIDTH_ESTIMATE_VERSION: u32 = 1;

const REPORT_MAGIC: &[u8; 4] = b"ETBW";
pub(crate) const WINDOW_REPORT_LEN: usize = 20;

pub(crate) fn encode_window_report(seq: u32, bps: u64, reply: bool) -> [u8; WINDOW_REPORT_LEN] {
    let mut payload = [0; WINDOW_REPORT_LEN];
    payload[..4].copy_from_slice(&seq.to_le_bytes());
    payload[4..8].copy_from_slice(REPORT_MAGIC);
    payload[8] = BANDWIDTH_ESTIMATE_VERSION as u8;
    payload[9] = u8::from(reply);
    payload[12..].copy_from_slice(&bps.to_le_bytes());
    payload
}

pub(crate) fn decode_window_report(payload: &[u8], reply: bool) -> Option<u64> {
    if payload.len() != WINDOW_REPORT_LEN
        || &payload[4..8] != REPORT_MAGIC
        || payload[8] != BANDWIDTH_ESTIMATE_VERSION as u8
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
