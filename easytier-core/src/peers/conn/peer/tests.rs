use std::{sync::Arc, time::Duration};

use crate::{
    packet::{PacketType, ZCPacket},
    peers::{
        PeerConnectionOrigin,
        conn::{peer_conn::PeerConn, peer_session::PeerSessionStore},
        context::{ArcPeerContext, NetworkIdentity, PeerContext},
        create_packet_recv_chan,
        test_support::NoopPeerContext,
    },
    proto::common::{FlagsInConfig, TunnelInfo},
    tunnel::ring::{RingTunnel, create_ring_socket_pair},
};

use super::{Peer, PeerConnCloseEvent, conn_latency_sort_key, disguised_tunnel_scheme};

use crate::peers::conn::peer_conn::PeerConnId;

#[test]
fn protocol_preference_wins_only_after_liveness_is_verified() {
    use super::preferred_conn_sort_key as key;
    assert!(key(50_000, true, 0) < key(1_000, false, 1));
    assert!(key(1_000, false, 1) < key(0, true, 0));
    assert!(key(1_000, false, 0) < key(50_000, true, 0));
}

#[test]
fn disguised_tunnel_type_detection_allows_resolution_prefixes() {
    assert_eq!(disguised_tunnel_scheme("wss"), Some("wss"));
    assert_eq!(disguised_tunnel_scheme("http-txt-wss"), Some("wss"));
    assert_eq!(disguised_tunnel_scheme("http3"), Some("http3"));
    assert_eq!(disguised_tunnel_scheme("tcp"), None);
    assert_eq!(disguised_tunnel_scheme("quic-http3-wrap"), None);
}

#[test]
fn measured_relay_precedes_unverified_hole_punch_path() {
    assert!(conn_latency_sort_key(20_000, false) < conn_latency_sort_key(0, true));
}

#[test]
fn unmeasured_regular_connection_keeps_existing_priority() {
    assert!(conn_latency_sort_key(0, false) < conn_latency_sort_key(20_000, false));
}

#[test]
fn verified_lower_latency_path_can_be_preferred() {
    assert!(conn_latency_sort_key(5_000, true) < conn_latency_sort_key(20_000, false));
}

async fn handshaken_connection(
    context: ArcPeerContext,
    scheme: &str,
    origin: PeerConnectionOrigin,
    is_client: bool,
) -> (PeerConn, PeerConn) {
    let remote_context = Arc::new(NoopPeerContext::default().with_flags(context.flags()));
    handshaken_connection_with_remote(context, remote_context, scheme, origin, is_client, None)
        .await
}

async fn handshaken_connection_with_remote(
    context: ArcPeerContext,
    remote_context: ArcPeerContext,
    scheme: &str,
    origin: PeerConnectionOrigin,
    is_client: bool,
    remote_features: Option<Vec<String>>,
) -> (PeerConn, PeerConn) {
    let remote_origin = match origin {
        PeerConnectionOrigin::Manual | PeerConnectionOrigin::Direct => {
            PeerConnectionOrigin::Listener
        }
        PeerConnectionOrigin::Listener => PeerConnectionOrigin::Direct,
        origin => origin,
    };
    handshaken_connection_with_origins(
        context,
        remote_context,
        scheme,
        origin,
        remote_origin,
        is_client,
        remote_features,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn handshaken_connection_with_origins(
    context: ArcPeerContext,
    remote_context: ArcPeerContext,
    scheme: &str,
    origin: PeerConnectionOrigin,
    remote_origin: PeerConnectionOrigin,
    is_client: bool,
    remote_features: Option<Vec<String>>,
) -> (PeerConn, PeerConn) {
    let (client_socket, server_socket) = create_ring_socket_pair(64);
    let tunnel_info = Some(TunnelInfo {
        tunnel_type: scheme.to_owned(),
        ..Default::default()
    });
    let session_store = Arc::new(PeerSessionStore::new());
    let mut local = PeerConn::new_with_peer_id_hint_and_origin(
        1,
        context,
        Box::new(RingTunnel::new(client_socket, tunnel_info.clone())),
        None,
        session_store.clone(),
        origin,
    );
    let mut remote = PeerConn::new_with_peer_id_hint_and_origin(
        2,
        remote_context,
        Box::new(RingTunnel::new(server_socket, tunnel_info)),
        None,
        session_store,
        remote_origin,
    );
    if let Some(features) = remote_features {
        remote.override_handshake_features(features);
    }
    let (local_result, remote_result) = if is_client {
        tokio::join!(
            local.do_handshake_as_client(),
            remote.do_handshake_as_server_ext(|_, _| Ok(())),
        )
    } else {
        tokio::join!(
            local.do_handshake_as_server_ext(|_, _| Ok(())),
            remote.do_handshake_as_client(),
        )
    };
    local_result.unwrap();
    remote_result.unwrap();
    (local, remote)
}

fn data_packet() -> ZCPacket {
    let mut packet = ZCPacket::new_with_payload(b"preferred-path");
    packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
    packet
}

/// Waits until every listed conn has answered its first ping (latency
/// above zero), the precondition for retiring another conn in favor of
/// it.
async fn wait_until_latency_verified(peer: &Peer, conn_ids: &[PeerConnId]) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let all_verified = conn_ids.iter().all(|conn_id| {
                peer.conns
                    .get(conn_id)
                    .is_some_and(|conn| conn.get_stats().latency_us > 0)
            });
            if all_verified {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("connections should complete their first ping round trip");
}

async fn check_preferred_transition(default_protocol: &str, use_disguise: bool, strict: bool) {
    let (preferred, fallback, punch_origin) = match (default_protocol, use_disguise) {
        ("udp", true) => ("http3", "wss", PeerConnectionOrigin::UdpHolePunch),
        ("tcp", true) => ("wss", "http3", PeerConnectionOrigin::TcpHolePunch),
        ("udp", false) => ("udp", "tcp", PeerConnectionOrigin::UdpHolePunch),
        ("tcp", false) => ("tcp", "udp", PeerConnectionOrigin::TcpHolePunch),
        _ => unreachable!(),
    };
    let context: ArcPeerContext = Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
        default_protocol: default_protocol.to_owned(),
        prefer_wss_http3_for_p2p: use_disguise && !strict,
        only_use_wss_http3_for_hole_punching: strict,
        close_redundant_conns_when_disguised: true,
        ..Default::default()
    }));
    let (local_tx, _local_rx) = create_packet_recv_chan();
    let peer = Peer::new(2, local_tx, context.clone());
    let (old, mut old_remote) = handshaken_connection(
        context.clone(),
        fallback,
        PeerConnectionOrigin::Direct,
        true,
    )
    .await;
    let old_id = old.get_conn_id();
    let (old_tx, mut old_rx) = create_packet_recv_chan();
    old_remote.start_recv_loop(old_tx).await;
    peer.add_peer_conn(old).await.unwrap();
    // Route metadata negotiated the disguise preference for this peer
    // (PeerMap notes it before querying).
    peer.note_negotiated_disguise(use_disguise);
    assert!(!peer.has_connection_at_least_as_preferred(preferred, Some(use_disguise)));

    let (replacement, mut replacement_remote) =
        handshaken_connection(context.clone(), preferred, punch_origin, true).await;
    let replacement_id = replacement.get_conn_id();
    peer.add_peer_conn(replacement).await.unwrap();
    assert!(peer.has_connection_at_least_as_preferred(preferred, Some(use_disguise)));
    assert!(peer.conns.contains_key(&old_id));
    assert_eq!(peer.get_default_conn_id(), old_id);
    peer.send_msg(data_packet()).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), old_rx.recv())
            .await
            .unwrap()
            .unwrap()
            .payload(),
        b"preferred-path"
    );

    let (replacement_tx, mut replacement_rx) = create_packet_recv_chan();
    replacement_remote.start_recv_loop(replacement_tx).await;
    // No further admission occurs after the replacement's first ping:
    // maintenance must notice it and retire the old connection itself.
    tokio::time::timeout(Duration::from_secs(7), async {
        while peer.conns.contains_key(&old_id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("verified preferred path should retire the fallback");
    assert_eq!(peer.get_default_conn_id(), replacement_id);
    assert!(
        peer.conns
            .get(&replacement_id)
            .unwrap()
            .get_stats()
            .latency_us
            > 0
    );
    peer.send_msg(data_packet()).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), replacement_rx.recv())
            .await
            .unwrap()
            .unwrap()
            .payload(),
        b"preferred-path"
    );

    drop(replacement_remote);
    tokio::time::timeout(Duration::from_secs(1), async {
        while peer.conns.contains_key(&replacement_id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!peer.has_connection_at_least_as_preferred(preferred, Some(use_disguise)));
    let (recovered, mut recovered_remote) =
        handshaken_connection(context, fallback, PeerConnectionOrigin::Direct, true).await;
    let (recovered_tx, mut recovered_rx) = create_packet_recv_chan();
    recovered_remote.start_recv_loop(recovered_tx).await;
    peer.add_peer_conn(recovered).await.unwrap();
    assert!(!peer.has_connection_at_least_as_preferred(preferred, Some(use_disguise)));
    peer.send_msg(data_packet()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), recovered_rx.recv())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn verified_preferred_protocol_retires_fallback_without_new_admission() {
    tokio::join!(
        check_preferred_transition("udp", true, false),
        check_preferred_transition("tcp", true, false),
        check_preferred_transition("udp", false, false),
        check_preferred_transition("tcp", false, false),
        check_preferred_transition("udp", true, true),
    );
}

async fn exchange_peer_data(
    a: &Peer,
    b: &Peer,
    a_rx: &mut crate::peers::PacketRecvChanReceiver,
    b_rx: &mut crate::peers::PacketRecvChanReceiver,
) {
    for (sender, receiver, from, to) in [(a, b_rx, 1, 2), (b, a_rx, 2, 1)] {
        let mut packet = ZCPacket::new_with_payload(b"cleanup-still-connected");
        packet.fill_peer_manager_hdr(from, to, PacketType::Data as u8);
        sender.send_msg(packet).await.unwrap();
        let received = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("peer must still receive data")
            .unwrap();
        assert_eq!(received.payload(), b"cleanup-still-connected");
    }
}

async fn check_dual_peer_cleanup(
    protocols: [&str; 2],
    cleanup: [bool; 2],
    use_disguise: bool,
    a_client_roles: [bool; 2],
) {
    let contexts = protocols
        .into_iter()
        .zip(cleanup)
        .map(|(protocol, enabled)| {
            Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
                default_protocol: protocol.to_owned(),
                prefer_wss_http3_for_p2p: use_disguise,
                close_redundant_conns_when_disguised: enabled,
                ..Default::default()
            })) as ArcPeerContext
        })
        .collect::<Vec<_>>();
    let (a_tx, mut a_rx) = create_packet_recv_chan();
    let (b_tx, mut b_rx) = create_packet_recv_chan();
    let a = Peer::new(2, a_tx, contexts[0].clone());
    let b = Peer::new(1, b_tx, contexts[1].clone());
    let schemes = if use_disguise {
        ["wss", "http3"]
    } else {
        ["tcp", "udp"]
    };
    let origins = [
        PeerConnectionOrigin::TcpHolePunch,
        PeerConnectionOrigin::UdpHolePunch,
    ];
    let mut a_ids = Vec::new();
    let mut b_ids = Vec::new();
    for index in 0..2 {
        let (a_conn, b_conn) = handshaken_connection_with_remote(
            contexts[0].clone(),
            contexts[1].clone(),
            schemes[index],
            origins[index],
            a_client_roles[index],
            None,
        )
        .await;
        a_ids.push(a_conn.get_conn_id());
        b_ids.push(b_conn.get_conn_id());
        a.add_peer_conn(a_conn).await.unwrap();
        b.add_peer_conn(b_conn).await.unwrap();
    }
    tokio::join!(
        wait_until_latency_verified(&a, &a_ids),
        wait_until_latency_verified(&b, &b_ids)
    );
    exchange_peer_data(&a, &b, &mut a_rx, &mut b_rx).await;
    a.note_negotiated_disguise(use_disguise);
    b.note_negotiated_disguise(use_disguise);
    tokio::join!(a.reconcile_connections(), b.reconcile_connections());
    if protocols[0] == protocols[1] && cleanup.into_iter().any(|enabled| enabled) {
        let preferred = usize::from(protocols[0] == "udp");
        tokio::time::timeout(Duration::from_secs(1), async {
            while a.conns.len() != 1 || b.conns.len() != 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("both endpoints must keep the agreed preferred path");
        assert!(a.conns.contains_key(&a_ids[preferred]));
        assert!(b.conns.contains_key(&b_ids[preferred]));
    } else {
        // Repeat both maintenance passes: preference conflicts must keep
        // both paths, including the mirror case where both candidates
        // happen to be clients rather than servers.
        for _ in 0..3 {
            tokio::join!(a.reconcile_connections(), b.reconcile_connections());
            tokio::task::yield_now().await;
        }
        assert_eq!(a.conns.len(), 2);
        assert_eq!(b.conns.len(), 2);
    }
    exchange_peer_data(&a, &b, &mut a_rx, &mut b_rx).await;
}

#[tokio::test]
async fn dual_peer_cleanup_preserves_conflicts_for_every_dial_direction() {
    for use_disguise in [false, true] {
        for roles in [[false, false], [false, true], [true, false], [true, true]] {
            check_dual_peer_cleanup(["tcp", "udp"], [true, true], use_disguise, roles).await;
        }
    }
}

#[tokio::test]
async fn dual_peer_agreed_cleanup_works_with_either_endpoint_enabled() {
    for use_disguise in [false, true] {
        for roles in [[false, false], [false, true], [true, false], [true, true]] {
            for enabled in [[false, false], [true, false], [false, true], [true, true]] {
                check_dual_peer_cleanup(["tcp", "tcp"], enabled, use_disguise, roles).await;
            }
            check_dual_peer_cleanup(["udp", "udp"], [true, true], use_disguise, roles).await;
        }
    }
}

async fn wait_for_single_connection(a: &Peer, b: &Peer) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while a.conns.len() != 1 || b.conns.len() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both endpoints must retain the same connection");
}

#[tokio::test]
async fn passive_disguise_retires_both_raw_transports_at_both_endpoints() {
    for scheme in ["wss", "http-txt-http3"] {
        for a_is_client in [false, true] {
            for cleanup in [[false, false], [true, false], [false, true], [true, true]] {
                // Even conflicting TCP/UDP preferences agree that an existing
                // disguised connection outranks both raw transports.
                let contexts = ["tcp", "udp"]
                    .into_iter()
                    .zip(cleanup)
                    .map(|(protocol, enabled)| {
                        Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
                            default_protocol: protocol.to_owned(),
                            close_redundant_conns_when_disguised: enabled,
                            ..Default::default()
                        })) as ArcPeerContext
                    })
                    .collect::<Vec<_>>();
                let (a_tx, mut a_rx) = create_packet_recv_chan();
                let (b_tx, mut b_rx) = create_packet_recv_chan();
                let a = Peer::new(2, a_tx, contexts[0].clone());
                let b = Peer::new(1, b_tx, contexts[1].clone());
                let mut a_ids = Vec::new();
                let mut b_ids = Vec::new();
                for transport in ["tcp", "udp", scheme] {
                    let (a_origin, b_origin) = if a_is_client {
                        (PeerConnectionOrigin::Direct, PeerConnectionOrigin::Listener)
                    } else {
                        (PeerConnectionOrigin::Listener, PeerConnectionOrigin::Direct)
                    };
                    let (a_conn, b_conn) = handshaken_connection_with_origins(
                        contexts[0].clone(),
                        contexts[1].clone(),
                        transport,
                        a_origin,
                        b_origin,
                        a_is_client,
                        None,
                    )
                    .await;
                    a_ids.push(a_conn.get_conn_id());
                    b_ids.push(b_conn.get_conn_id());
                    a.add_peer_conn(a_conn).await.unwrap();
                    b.add_peer_conn(b_conn).await.unwrap();
                }
                tokio::join!(
                    wait_until_latency_verified(&a, &a_ids),
                    wait_until_latency_verified(&b, &b_ids)
                );
                a.note_negotiated_disguise(false);
                b.note_negotiated_disguise(false);
                tokio::join!(a.reconcile_connections(), b.reconcile_connections());
                if cleanup.into_iter().any(|enabled| enabled) {
                    wait_for_single_connection(&a, &b).await;
                    assert!(a.conns.contains_key(&a_ids[2]));
                    assert!(b.conns.contains_key(&b_ids[2]));
                } else {
                    tokio::task::yield_now().await;
                    assert_eq!(a.conns.len(), 3);
                    assert_eq!(b.conns.len(), 3);
                }
                assert_eq!(a.select_conn().unwrap().get_conn_id(), a_ids[2]);
                assert_eq!(b.select_conn().unwrap().get_conn_id(), b_ids[2]);
                for peer in [&a, &b] {
                    assert!(peer.has_connection_at_least_as_preferred("tcp", Some(false)));
                    assert!(peer.has_connection_at_least_as_preferred("udp", Some(false)));
                }
                exchange_peer_data(&a, &b, &mut a_rx, &mut b_rx).await;
            }
        }
    }
}

#[tokio::test]
async fn manual_connections_agree_on_one_winner_without_route_metadata() {
    for cleanup in [[false, false], [true, false], [false, true], [true, true]] {
        let contexts = ["tcp", "udp"]
            .into_iter()
            .zip(cleanup)
            .map(|(protocol, enabled)| {
                Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
                    default_protocol: protocol.to_owned(),
                    close_redundant_conns_when_disguised: enabled,
                    ..Default::default()
                })) as ArcPeerContext
            })
            .collect::<Vec<_>>();
        let (a_tx, mut a_rx) = create_packet_recv_chan();
        let (b_tx, mut b_rx) = create_packet_recv_chan();
        let a = Peer::new(2, a_tx, contexts[0].clone());
        let b = Peer::new(1, b_tx, contexts[1].clone());
        let mut a_ids = Vec::new();
        let mut b_ids = Vec::new();
        let mut manual_keys = Vec::new();
        for (scheme, a_origin, b_origin, a_is_client) in [
            (
                "tcp",
                PeerConnectionOrigin::Direct,
                PeerConnectionOrigin::Listener,
                true,
            ),
            (
                "udp",
                PeerConnectionOrigin::UdpHolePunch,
                PeerConnectionOrigin::UdpHolePunch,
                false,
            ),
            (
                "wss",
                PeerConnectionOrigin::Listener,
                PeerConnectionOrigin::Direct,
                false,
            ),
            (
                "http3",
                PeerConnectionOrigin::UdpHolePunch,
                PeerConnectionOrigin::UdpHolePunch,
                true,
            ),
            (
                "ring",
                PeerConnectionOrigin::Attached,
                PeerConnectionOrigin::Attached,
                true,
            ),
            (
                "tcp",
                PeerConnectionOrigin::Manual,
                PeerConnectionOrigin::Listener,
                true,
            ),
            (
                "udp",
                PeerConnectionOrigin::Listener,
                PeerConnectionOrigin::Manual,
                false,
            ),
        ] {
            let (a_conn, b_conn) = handshaken_connection_with_origins(
                contexts[0].clone(),
                contexts[1].clone(),
                scheme,
                a_origin,
                b_origin,
                a_is_client,
                None,
            )
            .await;
            if a_conn.is_manual() {
                assert!(b_conn.is_manual());
                let key = a_conn.cleanup_policy().shared_connection_key().unwrap();
                assert_eq!(Some(key), b_conn.cleanup_policy().shared_connection_key());
                manual_keys.push((key, a_ids.len()));
            }
            a_ids.push(a_conn.get_conn_id());
            b_ids.push(b_conn.get_conn_id());
            a.add_peer_conn(a_conn).await.unwrap();
            b.add_peer_conn(b_conn).await.unwrap();
        }
        tokio::join!(
            wait_until_latency_verified(&a, &a_ids),
            wait_until_latency_verified(&b, &b_ids)
        );
        assert_eq!(a.negotiated_disguise.load(), None);
        assert_eq!(b.negotiated_disguise.load(), None);
        let winner = manual_keys.into_iter().min().unwrap().1;
        tokio::join!(a.reconcile_connections(), b.reconcile_connections());
        let enabled = cleanup.into_iter().any(|enabled| enabled);
        if enabled {
            wait_for_single_connection(&a, &b).await;
        } else {
            tokio::task::yield_now().await;
            assert_eq!(a.conns.len(), a_ids.len());
            assert_eq!(b.conns.len(), b_ids.len());
        }
        assert_eq!(a.select_conn().unwrap().get_conn_id(), a_ids[winner]);
        assert_eq!(b.select_conn().unwrap().get_conn_id(), b_ids[winner]);
        for peer in [&a, &b] {
            assert_eq!(peer.has_usable_manual_connection(), enabled);
            for target in ["tcp", "udp", "wss", "http3"] {
                assert!(peer.has_connection_at_least_as_preferred(target, None));
            }
        }
        exchange_peer_data(&a, &b, &mut a_rx, &mut b_rx).await;
        if enabled {
            a.close_peer_conn(&a_ids[winner]).await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while a.has_live_conns() || b.has_live_conns() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(!a.has_usable_manual_connection());
            assert!(!b.has_usable_manual_connection());
            assert!(!a.has_connection_at_least_as_preferred("wss", None));
        }
    }
}

#[tokio::test]
async fn manual_priority_waits_for_liveness_and_revalidates_queued_cleanup() {
    for fail_replacement in [false, true] {
        let flags = FlagsInConfig {
            default_protocol: "udp".to_owned(),
            close_redundant_conns_when_disguised: true,
            ..Default::default()
        };
        let context: ArcPeerContext = Arc::new(NoopPeerContext::default().with_flags(flags));
        let (tx, _rx) = create_packet_recv_chan();
        let peer = Peer::new(2, tx, context.clone());
        let (fallback, mut fallback_remote) =
            handshaken_connection(context.clone(), "http3", PeerConnectionOrigin::Direct, true)
                .await;
        let fallback_id = fallback.get_conn_id();
        let (fallback_tx, mut fallback_rx) = create_packet_recv_chan();
        fallback_remote.start_recv_loop(fallback_tx).await;
        peer.add_peer_conn(fallback).await.unwrap();
        wait_until_latency_verified(&peer, &[fallback_id]).await;
        let (manual, mut manual_remote) =
            handshaken_connection(context, "tcp", PeerConnectionOrigin::Manual, true).await;
        let manual_id = manual.get_conn_id();
        let policy = manual.cleanup_policy().clone();
        peer.add_peer_conn(manual).await.unwrap();
        peer.reconcile_connections().await;
        assert_eq!(peer.get_default_conn_id(), fallback_id);
        assert!(!peer.has_usable_manual_connection());
        peer.send_msg(data_packet()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), fallback_rx.recv())
                .await
                .unwrap()
                .is_some()
        );
        let (manual_tx, mut manual_rx) = create_packet_recv_chan();
        manual_remote.start_recv_loop(manual_tx).await;
        wait_until_latency_verified(&peer, &[manual_id]).await;
        if fail_replacement {
            peer.close_event_sender
                .try_send(PeerConnCloseEvent::Closed(manual_id))
                .unwrap();
        }
        peer.close_event_sender
            .try_send(PeerConnCloseEvent::Redundant {
                conn_id: fallback_id,
                replacement_id: manual_id,
                use_disguise: false,
                policy,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(peer.conns.contains_key(&fallback_id), fail_replacement);
        assert_eq!(peer.conns.contains_key(&manual_id), !fail_replacement);
        peer.reconcile_connections().await;
        peer.send_msg(data_packet()).await.unwrap();
        let rx = if fail_replacement {
            &mut fallback_rx
        } else {
            &mut manual_rx
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn manual_priority_does_not_retire_a_legacy_connection() {
    let context: ArcPeerContext = Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
        close_redundant_conns_when_disguised: true,
        ..Default::default()
    }));
    let (tx, _rx) = create_packet_recv_chan();
    let peer = Peer::new(2, tx, context.clone());
    let mut ids = Vec::new();
    let mut remotes = Vec::new();
    for (scheme, origin, features) in [
        (
            "wss",
            PeerConnectionOrigin::Direct,
            Some(vec!["p2p-cleanup-v1:tcp:0:0:0".to_owned()]),
        ),
        ("tcp", PeerConnectionOrigin::Manual, None),
    ] {
        let (conn, mut remote) = handshaken_connection_with_remote(
            context.clone(),
            context.clone(),
            scheme,
            origin,
            true,
            features,
        )
        .await;
        ids.push(conn.get_conn_id());
        let (remote_tx, remote_rx) = create_packet_recv_chan();
        remote.start_recv_loop(remote_tx).await;
        remotes.push((remote, remote_rx));
        peer.add_peer_conn(conn).await.unwrap();
    }
    wait_until_latency_verified(&peer, &ids).await;
    peer.reconcile_connections().await;
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(peer.conns.len(), 2);
    assert_eq!(peer.get_default_conn_id(), ids[1]);
    peer.send_msg(data_packet()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), remotes[1].1.recv())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn cleanup_keeps_noncooperating_peers_and_mismatched_connection_snapshots() {
    let flags = FlagsInConfig {
        default_protocol: "tcp".to_owned(),
        close_redundant_conns_when_disguised: true,
        ..Default::default()
    };
    let context: ArcPeerContext = Arc::new(NoopPeerContext::default().with_flags(flags.clone()));
    for remote_features in [
        vec!["liveness-echo-v1"],
        vec!["liveness-echo-v1", "p2p-cleanup-v2:tcp:0:0:0"],
        vec!["liveness-echo-v1", "p2p-cleanup-v1:tcp:bad:0:0"],
        vec![
            "liveness-echo-v1",
            "p2p-cleanup-v1:tcp:0:0:0",
            "p2p-cleanup-v1:tcp:0:0:0",
        ],
    ] {
        let remote_features = remote_features
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let (tx, mut rx) = create_packet_recv_chan();
        let peer = Peer::new(2, tx, context.clone());
        let mut ids = Vec::new();
        let mut remotes = Vec::new();
        for (scheme, origin) in [
            ("udp", PeerConnectionOrigin::UdpHolePunch),
            ("tcp", PeerConnectionOrigin::TcpHolePunch),
        ] {
            let (conn, mut remote) = handshaken_connection_with_remote(
                context.clone(),
                context.clone(),
                scheme,
                origin,
                true,
                Some(remote_features.clone()),
            )
            .await;
            let (remote_tx, remote_rx) = create_packet_recv_chan();
            remote.start_recv_loop(remote_tx).await;
            ids.push(conn.get_conn_id());
            peer.add_peer_conn(conn).await.unwrap();
            remotes.push((remote, remote_rx));
        }
        wait_until_latency_verified(&peer, &ids).await;
        peer.note_negotiated_disguise(false);
        peer.reconcile_connections().await;
        tokio::task::yield_now().await;
        assert_eq!(peer.conns.len(), 2, "{remote_features:?}");
        peer.send_msg(data_packet()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), remotes[1].1.recv())
                .await
                .unwrap()
                .is_some()
        );
        let mut reply = data_packet();
        reply.fill_peer_manager_hdr(2, 1, PacketType::Data as u8);
        remotes[1].0.send_msg(reply).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .is_some()
        );
    }

    // The remote changed its advertised preference between the two
    // connections. Even though both independently negotiate raw protocols,
    // they cannot replace each other using different paired snapshots.
    let (tx, _rx) = create_packet_recv_chan();
    let peer = Peer::new(2, tx, context.clone());
    let remote_context: ArcPeerContext =
        Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
            prefer_wss_http3_for_p2p: true,
            ..flags
        }));
    let mut ids = Vec::new();
    let mut remotes = Vec::new();
    for (scheme, remote_context) in [("udp", context.clone()), ("tcp", remote_context)] {
        let (conn, mut remote) = handshaken_connection_with_remote(
            context.clone(),
            remote_context,
            scheme,
            PeerConnectionOrigin::Direct,
            true,
            None,
        )
        .await;
        let (remote_tx, _remote_rx) = create_packet_recv_chan();
        remote.start_recv_loop(remote_tx).await;
        ids.push(conn.get_conn_id());
        peer.add_peer_conn(conn).await.unwrap();
        remotes.push(remote);
    }
    wait_until_latency_verified(&peer, &ids).await;
    peer.note_negotiated_disguise(false);
    peer.reconcile_connections().await;
    peer.close_event_sender
        .try_send(PeerConnCloseEvent::Redundant {
            conn_id: ids[0],
            replacement_id: ids[1],
            use_disguise: false,
            policy: peer.conns.get(&ids[1]).unwrap().cleanup_policy().clone(),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(peer.conns.len(), 2);
}

#[tokio::test]
async fn server_punch_cleanup_requires_enabled_flag_and_a_verified_better_protocol() {
    for cleanup_enabled in [false, true] {
        for (default_protocol, use_disguise, fallback, origin, preferred) in [
            (
                "udp",
                true,
                "wss",
                PeerConnectionOrigin::TcpHolePunch,
                "http3",
            ),
            (
                "tcp",
                true,
                "http3",
                PeerConnectionOrigin::UdpHolePunch,
                "wss",
            ),
            (
                "udp",
                false,
                "tcp",
                PeerConnectionOrigin::TcpHolePunch,
                "udp",
            ),
            (
                "tcp",
                false,
                "udp",
                PeerConnectionOrigin::UdpHolePunch,
                "tcp",
            ),
        ] {
            let context: ArcPeerContext =
                Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
                    default_protocol: default_protocol.to_owned(),
                    prefer_wss_http3_for_p2p: use_disguise,
                    close_redundant_conns_when_disguised: cleanup_enabled,
                    ..Default::default()
                }));
            let (local_tx, _local_rx) = create_packet_recv_chan();
            let peer = Peer::new(2, local_tx, context.clone());
            let (fallback_conn, mut fallback_remote) =
                handshaken_connection(context.clone(), fallback, origin, false).await;
            let fallback_id = fallback_conn.get_conn_id();
            let (fallback_tx, _fallback_rx) = create_packet_recv_chan();
            fallback_remote.start_recv_loop(fallback_tx).await;
            peer.add_peer_conn(fallback_conn).await.unwrap();
            wait_until_latency_verified(&peer, &[fallback_id]).await;

            let (preferred_conn, mut preferred_remote) =
                handshaken_connection(context, preferred, PeerConnectionOrigin::Direct, true).await;
            let preferred_id = preferred_conn.get_conn_id();
            let (preferred_tx, mut preferred_rx) = create_packet_recv_chan();
            // The lower-priority server survives before the replacement
            // has responded to a ping, even with cleanup enabled.
            peer.add_peer_conn(preferred_conn).await.unwrap();
            peer.note_negotiated_disguise(use_disguise);
            peer.reconcile_connections().await;
            tokio::task::yield_now().await;
            assert!(peer.conns.contains_key(&fallback_id));
            preferred_remote.start_recv_loop(preferred_tx).await;
            wait_until_latency_verified(&peer, &[preferred_id]).await;
            peer.reconcile_connections().await;

            if cleanup_enabled {
                tokio::time::timeout(Duration::from_secs(1), async {
                    while peer.conns.contains_key(&fallback_id) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
            } else {
                tokio::task::yield_now().await;
                assert!(peer.conns.contains_key(&fallback_id));
            }
            assert!(peer.conns.contains_key(&preferred_id));
            peer.send_msg(data_packet()).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(1), preferred_rx.recv())
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }
}

struct MutableCleanupContext(std::sync::Mutex<FlagsInConfig>);

impl PeerContext for MutableCleanupContext {
    fn network_identity(&self) -> NetworkIdentity {
        NetworkIdentity::default()
    }

    fn flags(&self) -> FlagsInConfig {
        self.0.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn later_handshakes_update_manual_reconnect_suppression_for_the_surviving_path() {
    let context: ArcPeerContext = Arc::new(NoopPeerContext::default());
    let remote_context = Arc::new(MutableCleanupContext(std::sync::Mutex::new(
        context.flags(),
    )));
    let (tx, _rx) = create_packet_recv_chan();
    let peer = Peer::new(2, tx, context.clone());
    let (manual, mut manual_remote) = handshaken_connection_with_remote(
        context.clone(),
        remote_context.clone(),
        "tcp",
        PeerConnectionOrigin::Manual,
        true,
        None,
    )
    .await;
    let manual_id = manual.get_conn_id();
    let (remote_tx, _remote_rx) = create_packet_recv_chan();
    manual_remote.start_recv_loop(remote_tx).await;
    peer.add_peer_conn(manual).await.unwrap();
    wait_until_latency_verified(&peer, &[manual_id]).await;
    assert!(!peer.has_usable_manual_connection());

    for enabled in [true, false, true] {
        remote_context
            .0
            .lock()
            .unwrap()
            .close_redundant_conns_when_disguised = enabled;
        let (later, mut later_remote) = handshaken_connection_with_remote(
            context.clone(),
            remote_context.clone(),
            "udp",
            PeerConnectionOrigin::Direct,
            true,
            None,
        )
        .await;
        let later_id = later.get_conn_id();
        let (later_tx, _later_rx) = create_packet_recv_chan();
        later_remote.start_recv_loop(later_tx).await;
        peer.add_peer_conn(later).await.unwrap();
        assert_eq!(peer.has_usable_manual_connection(), enabled);
        peer.close_peer_conn(&later_id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while peer.conns.contains_key(&later_id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(peer.has_usable_manual_connection(), enabled);
    }
    peer.close_peer_conn(&manual_id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while peer.has_live_conns() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!peer.has_usable_manual_connection());
    assert!(!peer.remote_cleanup_requested.load());
}

#[tokio::test]
async fn queued_cleanup_rechecks_every_local_ordering_input() {
    for changed_field in 0..4 {
        let flags = FlagsInConfig {
            default_protocol: "tcp".to_owned(),
            close_redundant_conns_when_disguised: true,
            ..Default::default()
        };
        let context = Arc::new(MutableCleanupContext(std::sync::Mutex::new(flags)));
        let (tx, _rx) = create_packet_recv_chan();
        let peer = Peer::new(2, tx, context.clone());
        let mut ids = Vec::new();
        let mut remotes = Vec::new();
        for scheme in ["udp", "tcp"] {
            let (conn, mut remote) =
                handshaken_connection(context.clone(), scheme, PeerConnectionOrigin::Direct, true)
                    .await;
            let (remote_tx, _remote_rx) = create_packet_recv_chan();
            remote.start_recv_loop(remote_tx).await;
            ids.push(conn.get_conn_id());
            peer.add_peer_conn(conn).await.unwrap();
            remotes.push(remote);
        }
        wait_until_latency_verified(&peer, &ids).await;
        peer.note_negotiated_disguise(false);
        peer.close_event_sender
            .try_send(PeerConnCloseEvent::Redundant {
                conn_id: ids[0],
                replacement_id: ids[1],
                use_disguise: false,
                policy: peer.conns.get(&ids[1]).unwrap().cleanup_policy().clone(),
            })
            .unwrap();
        // Mutate without yielding, after the event was queued. The local
        // handshake snapshot is fixed even if flags change concurrently.
        {
            let mut flags = context.0.lock().unwrap();
            match changed_field {
                0 => flags.default_protocol = "udp".to_owned(),
                1 => flags.prefer_wss_http3_for_p2p = true,
                2 => flags.only_use_wss_http3_for_hole_punching = true,
                _ => flags.disable_wss_http3_for_p2p = true,
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(peer.conns.len(), 2, "changed field {changed_field}");

        // Reconnect using the new policy. The stale handshakes above are
        // fenced, but fresh matching handshakes must restore cleanup.
        for id in &ids {
            peer.close_peer_conn(id).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while !peer.conns.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        remotes.clear();
        peer.negotiated_disguise.store(None);
        let use_disguise = matches!(changed_field, 1 | 2);
        let (fallback, preferred) = if use_disguise {
            ("http3", "wss")
        } else if changed_field == 0 {
            ("tcp", "udp")
        } else {
            ("udp", "tcp")
        };
        let mut fresh_ids = Vec::new();
        for scheme in [fallback, preferred] {
            let (conn, mut remote) =
                handshaken_connection(context.clone(), scheme, PeerConnectionOrigin::Direct, true)
                    .await;
            let (remote_tx, _remote_rx) = create_packet_recv_chan();
            remote.start_recv_loop(remote_tx).await;
            fresh_ids.push(conn.get_conn_id());
            peer.add_peer_conn(conn).await.unwrap();
            remotes.push(remote);
        }
        wait_until_latency_verified(&peer, &fresh_ids).await;
        peer.note_negotiated_disguise(use_disguise);
        peer.reconcile_connections().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while peer.conns.contains_key(&fresh_ids[0]) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fresh matching policy must restore cleanup");
        assert!(peer.conns.contains_key(&fresh_ids[1]));
    }
}

#[tokio::test]
async fn cleanup_waits_for_route_cache_to_match_the_handshake_disguise_policy() {
    let context: ArcPeerContext = Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
        default_protocol: "tcp".to_owned(),
        prefer_wss_http3_for_p2p: true,
        close_redundant_conns_when_disguised: true,
        ..Default::default()
    }));
    let (tx, _rx) = create_packet_recv_chan();
    let peer = Peer::new(2, tx, context.clone());
    let mut ids = Vec::new();
    let mut remotes = Vec::new();
    for scheme in ["tcp", "wss"] {
        let (conn, mut remote) = handshaken_connection(
            context.clone(),
            scheme,
            PeerConnectionOrigin::TcpHolePunch,
            true,
        )
        .await;
        let (remote_tx, _remote_rx) = create_packet_recv_chan();
        remote.start_recv_loop(remote_tx).await;
        ids.push(conn.get_conn_id());
        peer.add_peer_conn(conn).await.unwrap();
        remotes.push(remote);
    }
    wait_until_latency_verified(&peer, &ids).await;
    peer.note_negotiated_disguise(false);
    peer.reconcile_connections().await;
    peer.close_event_sender
        .try_send(PeerConnCloseEvent::Redundant {
            conn_id: ids[1],
            replacement_id: ids[0],
            use_disguise: false,
            policy: peer.conns.get(&ids[0]).unwrap().cleanup_policy().clone(),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(peer.conns.len(), 2);

    peer.note_negotiated_disguise(true);
    peer.reconcile_connections().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while peer.conns.contains_key(&ids[0]) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(peer.conns.contains_key(&ids[1]));
}

#[tokio::test]
async fn queued_server_punch_cleanup_is_cancelled_when_the_flag_is_disabled() {
    let context = Arc::new(MutableCleanupContext(std::sync::Mutex::new(
        FlagsInConfig {
            default_protocol: "udp".to_owned(),
            close_redundant_conns_when_disguised: true,
            ..Default::default()
        },
    )));
    let (tx, _rx) = create_packet_recv_chan();
    let peer = Peer::new(2, tx, context.clone());
    let (fallback, mut fallback_remote) = handshaken_connection(
        context.clone(),
        "tcp",
        PeerConnectionOrigin::TcpHolePunch,
        false,
    )
    .await;
    let fallback_id = fallback.get_conn_id();
    let (fallback_tx, _fallback_rx) = create_packet_recv_chan();
    fallback_remote.start_recv_loop(fallback_tx).await;
    peer.add_peer_conn(fallback).await.unwrap();
    let (preferred, mut preferred_remote) =
        handshaken_connection(context.clone(), "udp", PeerConnectionOrigin::Direct, true).await;
    let preferred_id = preferred.get_conn_id();
    let (preferred_tx, _preferred_rx) = create_packet_recv_chan();
    preferred_remote.start_recv_loop(preferred_tx).await;
    peer.add_peer_conn(preferred).await.unwrap();
    wait_until_latency_verified(&peer, &[fallback_id, preferred_id]).await;
    peer.note_negotiated_disguise(false);

    peer.close_event_sender
        .try_send(PeerConnCloseEvent::Redundant {
            conn_id: fallback_id,
            replacement_id: preferred_id,
            use_disguise: false,
            policy: peer
                .conns
                .get(&preferred_id)
                .unwrap()
                .cleanup_policy()
                .clone(),
        })
        .unwrap();
    // No await between enqueue and this live config change: the close
    // worker must re-read the flag before removing the server connection.
    context
        .0
        .lock()
        .unwrap()
        .close_redundant_conns_when_disguised = false;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(peer.conns.contains_key(&fallback_id));
    assert!(peer.conns.contains_key(&preferred_id));
}

#[tokio::test]
async fn failed_unverified_preferred_path_keeps_working_fallback() {
    let context: ArcPeerContext = Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
        default_protocol: "udp".to_owned(),
        prefer_wss_http3_for_p2p: true,
        close_redundant_conns_when_disguised: true,
        ..Default::default()
    }));
    let (local_tx, _local_rx) = create_packet_recv_chan();
    let peer = Peer::new(2, local_tx, context.clone());
    let (fallback, mut fallback_remote) = handshaken_connection(
        context.clone(),
        "wss",
        PeerConnectionOrigin::TcpHolePunch,
        false,
    )
    .await;
    let fallback_id = fallback.get_conn_id();
    let (remote_tx, mut remote_rx) = create_packet_recv_chan();
    fallback_remote.start_recv_loop(remote_tx).await;
    peer.add_peer_conn(fallback).await.unwrap();
    assert!(!peer.has_connection_at_least_as_preferred("http3", Some(true)));
    let (replacement, replacement_remote) =
        handshaken_connection(context, "http3", PeerConnectionOrigin::UdpHolePunch, true).await;
    let replacement_id = replacement.get_conn_id();
    peer.add_peer_conn(replacement).await.unwrap();
    drop(replacement_remote);
    tokio::time::timeout(Duration::from_secs(1), async {
        while peer.conns.contains_key(&replacement_id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    peer.reconcile_connections().await;
    assert!(peer.conns.contains_key(&fallback_id));
    assert!(!peer.has_connection_at_least_as_preferred("http3", Some(true)));
    peer.send_msg(data_packet()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), remote_rx.recv())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn queued_cleanup_rechecks_policy_and_replacement_at_removal() {
    for preference_changed in [false, true] {
        let context: ArcPeerContext =
            Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
                default_protocol: "udp".to_owned(),
                prefer_wss_http3_for_p2p: true,
                close_redundant_conns_when_disguised: true,
                ..Default::default()
            }));
        let (tx, _rx) = create_packet_recv_chan();
        let peer = Peer::new(2, tx, context.clone());
        let (udp, mut udp_remote) =
            handshaken_connection(context.clone(), "udp", PeerConnectionOrigin::Direct, true).await;
        let udp_id = udp.get_conn_id();
        let (wss, mut wss_remote) =
            handshaken_connection(context, "wss", PeerConnectionOrigin::TcpHolePunch, false).await;
        let wss_id = wss.get_conn_id();
        // Retiring a conn requires a verified replacement (first ping
        // answered), so let both conns run their ping round trips.
        let (udp_tx, _udp_rx) = create_packet_recv_chan();
        udp_remote.start_recv_loop(udp_tx).await;
        let (wss_tx, _wss_rx) = create_packet_recv_chan();
        wss_remote.start_recv_loop(wss_tx).await;
        peer.add_peer_conn(udp).await.unwrap();
        peer.add_peer_conn(wss).await.unwrap();
        wait_until_latency_verified(&peer, &[udp_id, wss_id]).await;
        peer.note_negotiated_disguise(false);
        if !preference_changed {
            peer.close_event_sender
                .try_send(PeerConnCloseEvent::Closed(udp_id))
                .unwrap();
        }
        peer.close_event_sender
            .try_send(PeerConnCloseEvent::Redundant {
                conn_id: wss_id,
                replacement_id: udp_id,
                use_disguise: false,
                policy: peer.conns.get(&udp_id).unwrap().cleanup_policy().clone(),
            })
            .unwrap();
        if preference_changed {
            peer.note_negotiated_disguise(true);
            peer.close_event_sender
                .try_send(PeerConnCloseEvent::Redundant {
                    conn_id: udp_id,
                    replacement_id: wss_id,
                    use_disguise: true,
                    policy: peer.conns.get(&wss_id).unwrap().cleanup_policy().clone(),
                })
                .unwrap();
        }
        tokio::task::yield_now().await;
        assert!(peer.conns.contains_key(&wss_id));
        assert!(!peer.conns.contains_key(&udp_id));
        assert_eq!(peer.select_conn().unwrap().get_conn_id(), wss_id);
    }
}

#[tokio::test]
async fn automatic_cleanup_preserves_attached_and_equal_rank_connections() {
    let context: ArcPeerContext = Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
        default_protocol: "udp".to_owned(),
        close_redundant_conns_when_disguised: true,
        ..Default::default()
    }));
    let (local_tx, _local_rx) = create_packet_recv_chan();
    let peer = Peer::new(2, local_tx, context.clone());
    peer.note_negotiated_disguise(false);
    let mut remote_conns = Vec::new();
    let mut kept_ids = Vec::new();
    for (scheme, origin, is_client) in [
        ("udp", PeerConnectionOrigin::Listener, false),
        ("udp", PeerConnectionOrigin::Direct, true),
        ("udp", PeerConnectionOrigin::Direct, true),
        ("udp", PeerConnectionOrigin::UdpHolePunch, false),
        ("ring", PeerConnectionOrigin::Attached, true),
    ] {
        let (conn, mut remote) =
            handshaken_connection(context.clone(), scheme, origin, is_client).await;
        let conn_id = conn.get_conn_id();
        let (remote_tx, _remote_rx) = create_packet_recv_chan();
        remote.start_recv_loop(remote_tx).await;
        remote_conns.push(remote);
        peer.add_peer_conn(conn).await.unwrap();
        wait_until_latency_verified(&peer, &[conn_id]).await;
        kept_ids.push(conn_id);
    }
    peer.reconcile_connections().await;
    tokio::task::yield_now().await;
    assert_eq!(peer.conns.len(), kept_ids.len());
    for id in kept_ids {
        assert!(peer.conns.contains_key(&id));
    }
}

#[tokio::test]
async fn established_disguise_precedes_raw_despite_passive_negotiation() {
    for cleanup_enabled in [false, true] {
        let context: ArcPeerContext =
            Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
                default_protocol: "udp".to_owned(),
                prefer_wss_http3_for_p2p: true,
                close_redundant_conns_when_disguised: cleanup_enabled,
                ..Default::default()
            }));
        let (local_tx, _local_rx) = create_packet_recv_chan();
        let peer = Peer::new(2, local_tx, context.clone());
        let remote_context: ArcPeerContext =
            Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
                prefer_wss_http3_for_p2p: false,
                ..context.flags()
            }));
        let (wss, mut wss_remote) = handshaken_connection_with_remote(
            context.clone(),
            remote_context.clone(),
            "wss",
            PeerConnectionOrigin::Direct,
            true,
            None,
        )
        .await;
        let wss_id = wss.get_conn_id();
        let (wss_tx, _wss_rx) = create_packet_recv_chan();
        wss_remote.start_recv_loop(wss_tx).await;
        peer.add_peer_conn(wss).await.unwrap();
        let (udp, mut udp_remote) = handshaken_connection_with_remote(
            context,
            remote_context,
            "udp",
            PeerConnectionOrigin::Direct,
            true,
            None,
        )
        .await;
        let udp_id = udp.get_conn_id();
        let (udp_tx, _udp_rx) = create_packet_recv_chan();
        udp_remote.start_recv_loop(udp_tx).await;
        peer.add_peer_conn(udp).await.unwrap();
        wait_until_latency_verified(&peer, &[udp_id, wss_id]).await;
        assert!(peer.conns.contains_key(&wss_id));
        assert!(peer.conns.contains_key(&udp_id));
        // Passive negotiation still controls dialing, but an established
        // disguised connection must not be torn down in favor of raw UDP.
        peer.note_negotiated_disguise(false);
        assert!(peer.has_connection_at_least_as_preferred("udp", Some(false)));
        assert_eq!(peer.select_conn().unwrap().get_conn_id(), wss_id);
        peer.reconcile_connections().await;
        assert_eq!(peer.get_default_conn_id(), wss_id);
        if cleanup_enabled {
            tokio::time::timeout(Duration::from_secs(1), async {
                while peer.conns.contains_key(&udp_id) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(peer.conns.contains_key(&udp_id), !cleanup_enabled);
        assert!(peer.conns.contains_key(&wss_id));
    }
}

#[tokio::test]
async fn missing_route_metadata_keeps_negotiated_disguise_stable() {
    let context: ArcPeerContext = Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
        default_protocol: "udp".to_owned(),
        prefer_wss_http3_for_p2p: true,
        close_redundant_conns_when_disguised: true,
        ..Default::default()
    }));
    let (local_tx, _local_rx) = create_packet_recv_chan();
    let peer = Peer::new(2, local_tx, context.clone());
    let (http3, _remote) =
        handshaken_connection(context.clone(), "http3", PeerConnectionOrigin::Direct, true).await;
    let http3_id = http3.get_conn_id();
    peer.add_peer_conn(http3).await.unwrap();
    // Authoritative metadata records the negotiated disguise preference
    // (PeerMap does this via note_negotiated_disguise before its query).
    peer.note_negotiated_disguise(true);
    assert!(peer.has_connection_at_least_as_preferred("http3", Some(true)));
    assert_eq!(peer.negotiated_disguise.load(), Some(true));
    // Missing route metadata must not flip it back to false, otherwise
    // the next reconcile pass would treat the established http3 path as
    // redundant and close it (connection churn on metadata flaps).
    assert!(peer.has_connection_at_least_as_preferred("udp", None));
    assert_eq!(peer.negotiated_disguise.load(), Some(true));
    peer.reconcile_connections().await;
    assert!(peer.conns.contains_key(&http3_id));
}

#[tokio::test]
async fn attached_ring_does_not_satisfy_network_transport_preference() {
    let context: ArcPeerContext = Arc::new(NoopPeerContext::default());
    let (local_tx, _local_rx) = create_packet_recv_chan();
    let peer = Peer::new(2, local_tx, context.clone());
    let (conn, _remote) =
        handshaken_connection(context, "ring", PeerConnectionOrigin::Attached, true).await;
    peer.add_peer_conn(conn).await.unwrap();
    assert!(!peer.has_connection_at_least_as_preferred("udp", Some(false)));
    assert!(!peer.has_connection_at_least_as_preferred("http3", Some(true)));
}
