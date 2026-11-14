//! Fork tests for UDP FEC, kept out of the upstream `tests` module.

use super::*;
use crate::peers::conn::parallel;

fn striped(id: u8, seq: u64) -> ZCPacket {
    parallel::wrap(source(id, 64), 11, 22, uuid::Uuid::from_u128(7), seq, 0)
}

#[tokio::test]
async fn striped_data_is_protected_like_the_packet_it_wraps() {
    let (left, right) = udp_pair().await;
    let left = Box::new(TunnelWithFilter::new(left, DropSources(vec![0])));
    let (left_io, left_fec) = active_io(left);
    let (mut right_io, right_fec) = active_io(right);
    let mut stream = right_io.get_stream();
    let original = striped(5, 1);
    assert_eq!(
        original.peer_manager_header().unwrap().packet_type,
        PacketType::ParallelLossyData as u8
    );
    left_io.get_sink().send(original.clone()).await.unwrap();
    assert_eq!(
        next(&mut stream).await.tunnel_payload(),
        original.tunnel_payload()
    );
    assert_eq!(right_fec.stats().recovered, 1);
    assert_eq!(left_fec.stats().tx_sources, 1);
}

#[test]
fn striped_proxy_traffic_follows_the_proxy_classes() {
    let mut proxy = source(1, 64);
    proxy.mut_peer_manager_header().unwrap().packet_type = PacketType::KcpSrc as u8;
    let proxy = parallel::wrap(proxy, 11, 22, uuid::Uuid::from_u128(7), 1, 0);
    assert_eq!(
        proxy.peer_manager_header().unwrap().packet_type,
        PacketType::ParallelData as u8
    );
    for (kcp, expected) in [(false, false), (true, true)] {
        let fec = PeerConnFec::new(
            &Flags {
                enable_udp_fec: true,
                enable_udp_fec_kcp: kcp,
                ..Default::default()
            },
            None,
        );
        fec.set_remote_features(&[FEATURE.to_owned()]);
        assert_eq!(fec.should_encode(&proxy), expected);
        assert!(fec.should_encode(&striped(2, 2)));
    }
    assert!(wire::eligible_peer(proxy.tunnel_payload()));
}
