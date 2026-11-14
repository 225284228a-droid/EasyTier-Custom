//! Which stream of a multi-stream tunnel a packet travels on.
//!
//! A reliable stream delivers in order, so one lost packet holds back every
//! packet behind it until the retransmission arrives. Spreading flows over
//! several streams of one connection limits that wait to the flows sharing
//! the stream: a download over many TCP connections keeps most of them
//! moving while one waits.
//!
//! Lane 0 carries control traffic. Data lanes hash the flow from local
//! metadata, which survives encryption of the payload and the parallel
//! envelope, so a flow keeps its order on one lane whether or not it is
//! striped; the stripe receiver orders per flow too. Data without a flow (IP
//! fragments, relayed packets) and proxy traffic hash the peer pair instead.

use std::hash::{DefaultHasher, Hash, Hasher};

use crate::packet::{PacketType, ZCPacket, flow::parse_flow_key};

pub const MAX_LANES: usize = 32;

/// Records the flow of an unencrypted data packet, as the send path does
/// before a packet reaches its connection.
pub fn mark_flow(packet: &mut ZCPacket) {
    packet.flow_key = parse_flow_key(packet);
}

/// The lane of `packet` on a tunnel with `lanes` streams.
pub fn lane_of(packet: &ZCPacket, lanes: usize) -> usize {
    if lanes <= 1 {
        return 0;
    }
    let Some(header) = packet.peer_manager_header() else {
        return 0;
    };
    let kind = header.packet_type;
    let mut hasher = DefaultHasher::new();
    let flow_data = [
        PacketType::Data,
        PacketType::ParallelData,
        PacketType::ParallelLossyData,
    ]
    .iter()
    .any(|candidate| *candidate as u8 == kind);
    if flow_data && let Some(flow) = packet.flow_key {
        flow.hash(&mut hasher);
    } else if [
        PacketType::Data,
        PacketType::DataWithKcpSrcModified,
        PacketType::DataWithQuicSrcModified,
        PacketType::KcpSrc,
        PacketType::KcpDst,
        PacketType::QuicSrc,
        PacketType::QuicDst,
        PacketType::ForeignNetworkPacket,
        PacketType::ParallelData,
        PacketType::ParallelLossyData,
    ]
    .iter()
    .any(|candidate| *candidate as u8 == kind)
    {
        (kind, header.from_peer_id.get(), header.to_peer_id.get()).hash(&mut hasher);
    } else {
        return 0;
    }
    1 + (hasher.finish() % (lanes as u64 - 1)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn udp(src_port: u16) -> ZCPacket {
        let mut bytes = vec![0; 28];
        bytes[0] = 0x45;
        bytes[2..4].copy_from_slice(&28u16.to_be_bytes());
        bytes[9] = 17;
        bytes[12..16].copy_from_slice(&[10, 0, 0, 1]);
        bytes[16..20].copy_from_slice(&[10, 0, 0, 2]);
        bytes[20..22].copy_from_slice(&src_port.to_be_bytes());
        bytes[22..24].copy_from_slice(&53u16.to_be_bytes());
        let mut packet = ZCPacket::new_with_payload(&bytes);
        packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        packet.flow_key = parse_flow_key(&packet);
        packet
    }

    fn of_type(kind: PacketType) -> ZCPacket {
        let mut packet = ZCPacket::new_with_payload(&[0; 32]);
        packet.fill_peer_manager_hdr(1, 2, kind as u8);
        packet
    }

    #[test]
    fn a_flow_keeps_one_data_lane_and_flows_spread_over_all_of_them() {
        let mut used = std::collections::BTreeSet::new();
        for port in 1000..1200 {
            let lane = lane_of(&udp(port), 8);
            assert!((1..8).contains(&lane));
            assert_eq!(lane_of(&udp(port), 8), lane);
            used.insert(lane);
        }
        assert_eq!(used.len(), 7);
        assert_eq!(lane_of(&udp(1000), 1), 0);
        assert_eq!(lane_of(&udp(1000), 2), 1);
    }

    #[test]
    fn a_striped_flow_uses_the_lane_of_its_unstriped_packets() {
        for port in 1000..1050 {
            let plain = udp(port);
            let striped = crate::peers::conn::parallel::wrap(
                plain.clone(),
                1,
                2,
                uuid::Uuid::from_u128(1),
                9,
                0,
            );
            assert_eq!(lane_of(&striped, 8), lane_of(&plain, 8));
        }
    }

    #[test]
    fn control_stays_on_lane_zero_and_data_leaves_it() {
        for kind in [
            PacketType::RpcReq,
            PacketType::RpcResp,
            PacketType::Route,
            PacketType::RoutePacket,
            PacketType::Ping,
            PacketType::Pong,
            PacketType::HandShake,
            PacketType::NoiseHandshakeMsg1,
            PacketType::ParallelControl,
        ] {
            assert_eq!(lane_of(&of_type(kind), 8), 0, "{kind:?}");
        }
        // Without a flow, data still leaves the control lane, by peer pair.
        for kind in [
            PacketType::Data,
            PacketType::KcpSrc,
            PacketType::QuicDst,
            PacketType::ParallelLossyData,
        ] {
            let lane = lane_of(&of_type(kind), 8);
            assert!((1..8).contains(&lane), "{kind:?}");
        }
    }
}
