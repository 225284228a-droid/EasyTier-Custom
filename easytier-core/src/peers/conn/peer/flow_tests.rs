use super::*;
use crate::{
    packet::PacketType,
    peers::{
        PeerConnectionOrigin, conn::peer_session::PeerSessionStore, create_packet_recv_chan,
        recv_packet_envelope_from_chan, test_support::NoopPeerContext,
    },
    tunnel::ring::create_ring_tunnel_pair,
};

fn flow(port: u16) -> FlowKey {
    FlowKey {
        src: ([10, 0, 0, 1], port).into(),
        dst: ([10, 0, 0, 2], 5000).into(),
        protocol: 17,
    }
}

async fn add_connection(
    peer: &Peer,
    recv: PacketRecvChan,
    rtt: Option<u32>,
) -> (PeerConnId, PeerConn) {
    let (left, right) = create_ring_tunnel_pair();
    let sessions = Arc::new(PeerSessionStore::new());
    let context = Arc::new(NoopPeerContext::default());
    // Manual connections are ordered by connection identity before RTT, so
    // both ends agree. Direct connections compare by RTT, as upstream does.
    let mut client = PeerConn::new_with_peer_id_hint_and_origin(
        1,
        context.clone(),
        left,
        None,
        sessions.clone(),
        PeerConnectionOrigin::Direct,
    );
    let mut server = PeerConn::new_with_peer_id_hint_and_origin(
        2,
        context,
        right,
        None,
        sessions,
        PeerConnectionOrigin::Listener,
    );
    let (left, right) = tokio::join!(
        client.do_handshake_as_client(),
        server.do_handshake_as_server_ext(|_, _| Ok(())),
    );
    left.unwrap();
    right.unwrap();
    server.start_recv_loop(recv).await;
    if let Some(rtt) = rtt {
        client.record_latency_for_test(rtt);
    }
    let id = client.get_conn_id();
    peer.add_peer_conn(client).await.unwrap();
    (id, server)
}

fn new_peer() -> Peer {
    let (sender, _receiver) = create_packet_recv_chan();
    Peer::new(2, sender, Arc::new(NoopPeerContext::default()))
}

#[tokio::test]
async fn send_failure_marks_connection_closed_without_waiting_for_ping() {
    let (left, right) = create_ring_tunnel_pair();
    let (stream, _sink) = right.split();
    drop(stream);
    let conn = PeerConn::new(
        1,
        Arc::new(NoopPeerContext::default()),
        left,
        Arc::new(PeerSessionStore::new()),
    );
    crate::foundation::time::timeout(Duration::from_secs(1), async {
        loop {
            let mut packet = ZCPacket::new_with_payload(b"closed sink");
            packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
            if conn.send_msg(packet).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(conn.is_closed());
}

async fn packet_skips_failed_connection_before_cache_cleanup(flow_key: Option<FlowKey>) {
    let peer = new_peer();
    let (sender, mut receiver) = create_packet_recv_chan();
    let (old, old_server) = add_connection(&peer, sender.clone(), Some(20_000)).await;
    let (_, new_server) = add_connection(&peer, sender, Some(1_000)).await;
    let old_conn = peer.conns.get(&old).unwrap().value().clone();
    peer.default_conn.store(Some(old_conn.clone()));
    if let Some(key) = flow_key {
        assert_eq!(peer.select_flow_conn(key).unwrap().get_conn_id(), old);
    }
    // Keep the stale cache entry so the result cannot depend on task ordering.
    peer.close_event_listener.abort();
    drop(old_server);
    crate::foundation::time::timeout(Duration::from_secs(1), async {
        loop {
            let mut packet = ZCPacket::new_with_payload(b"closed sink");
            packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
            if old_conn.send_msg(packet).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(old_conn.is_closed());
    assert_eq!(peer.get_default_conn_id(), old);

    let mut packet = ZCPacket::new_with_payload(b"packet after failure");
    packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
    packet.flow_key = flow_key;
    peer.send_msg(packet).await.unwrap();
    let envelope = crate::foundation::time::timeout(
        Duration::from_secs(1),
        recv_packet_envelope_from_chan(&mut receiver),
    )
    .await
    .unwrap()
    .unwrap();
    let (packet, ingress) = envelope.into_parts();
    assert_eq!(packet.payload(), b"packet after failure");
    assert_eq!(
        ingress.peer_connection().unwrap().1,
        new_server.get_conn_id()
    );
}

#[tokio::test]
async fn unkeyed_packet_skips_failed_connection_before_cache_cleanup() {
    packet_skips_failed_connection_before_cache_cleanup(None).await;
}

#[tokio::test]
async fn active_flow_skips_failed_connection_before_cache_cleanup() {
    packet_skips_failed_connection_before_cache_cleanup(Some(flow(1234))).await;
}

fn distinct_flow(peer: &Peer, existing: FlowKey) -> FlowKey {
    let flows = peer.flow_bindings.lock();
    (1..=u16::MAX)
        .map(flow)
        .find(|key| flows.bucket(*key) != flows.bucket(existing))
        .unwrap()
}

#[tokio::test]
async fn active_flow_survives_new_connection_and_periodic_reselection() {
    let peer = new_peer();
    let (sender, mut receiver) = create_packet_recv_chan();
    let (old, old_server) = add_connection(&peer, sender.clone(), Some(20_000)).await;
    let key = flow(1234);
    assert_eq!(peer.select_flow_conn(key).unwrap().get_conn_id(), old);
    let (new, new_server) = add_connection(&peer, sender, Some(1_000)).await;
    // A slow but open connection must keep its existing flow.
    for _ in 0..15 {
        peer.conns
            .get(&old)
            .unwrap()
            .record_latency_for_test(500_000);
    }
    peer.default_conn.store(None);
    assert_eq!(peer.select_flow_conn(key).unwrap().get_conn_id(), old);
    let new_key = distinct_flow(&peer, key);
    assert_eq!(peer.select_flow_conn(new_key).unwrap().get_conn_id(), new);

    for (key, expected) in [
        (key, old_server.get_conn_id()),
        (new_key, new_server.get_conn_id()),
    ] {
        let mut packet = ZCPacket::new_with_payload(b"business data");
        packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        packet.flow_key = Some(key);
        peer.send_msg(packet).await.unwrap();
        let envelope = crate::foundation::time::timeout(
            Duration::from_secs(1),
            recv_packet_envelope_from_chan(&mut receiver),
        )
        .await
        .unwrap()
        .unwrap();
        let (packet, ingress) = envelope.into_parts();
        assert_eq!(packet.payload(), b"business data");
        assert_eq!(ingress.peer_connection().unwrap().1, expected);
    }
}

#[tokio::test]
async fn idle_flow_can_rebind_and_closed_connection_cannot_trap_active_flow() {
    let peer = new_peer();
    let (sender, _receiver) = create_packet_recv_chan();
    let (old, _old_server) = add_connection(&peer, sender.clone(), Some(20_000)).await;
    let key = flow(1234);
    assert_eq!(peer.select_flow_conn(key).unwrap().get_conn_id(), old);
    let (new, _new_server) = add_connection(&peer, sender, Some(1_000)).await;
    peer.default_conn.store(None);
    // Activity renews the idle timeout even across repeated intervals.
    for _ in 0..2 {
        {
            let mut flows = peer.flow_bindings.lock();
            let bucket = flows.bucket(key);
            flows.buckets.get_mut(&bucket).unwrap().last_active -=
                FlowBindings::IDLE_TIMEOUT - Duration::from_secs(1);
        }
        assert_eq!(peer.select_flow_conn(key).unwrap().get_conn_id(), old);
    }
    {
        let mut flows = peer.flow_bindings.lock();
        let bucket = flows.bucket(key);
        flows.buckets.get_mut(&bucket).unwrap().last_active -= FlowBindings::IDLE_TIMEOUT;
    }
    assert_eq!(peer.select_flow_conn(key).unwrap().get_conn_id(), new);
    peer.close_peer_conn(&new).await.unwrap();
    crate::foundation::time::timeout(Duration::from_secs(1), async {
        while peer.conns.contains_key(&new) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(peer.select_flow_conn(key).unwrap().get_conn_id(), old);
}

#[tokio::test]
async fn bucket_collisions_and_capacity_never_evict_an_active_binding() {
    let peer = new_peer();
    let (sender, _receiver) = create_packet_recv_chan();
    let (old, _old_server) = add_connection(&peer, sender.clone(), Some(20_000)).await;
    for port in 1..=u16::MAX {
        assert_eq!(
            peer.select_flow_conn(flow(port)).unwrap().get_conn_id(),
            old
        );
    }
    assert!(peer.flow_bindings.lock().buckets.len() <= FlowBindings::BUCKETS);
    let (_new, _new_server) = add_connection(&peer, sender, Some(1_000)).await;
    peer.default_conn.store(None);
    for port in 1..=u16::MAX {
        assert_eq!(
            peer.select_flow_conn(flow(port)).unwrap().get_conn_id(),
            old
        );
    }
}
