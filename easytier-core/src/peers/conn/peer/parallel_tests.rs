use super::super::{parallel::ParallelMemberToken, peer_session::PeerSessionStore};
use super::*;
use crate::{
    packet::PacketType,
    peers::{
        PacketRecvChanReceiver, PeerConnectionOrigin,
        context::{NetworkIdentity, PeerContext},
        create_packet_recv_chan,
    },
    proto::common::TunnelInfo,
    tunnel::{
        SplitTunnel, Tunnel,
        ring::{RingTunnel, create_ring_socket_pair},
    },
};
use futures::StreamExt;
use std::time::{Duration, Instant};

struct ParallelContext {
    count: std::sync::atomic::AtomicU32,
    disabled: std::sync::atomic::AtomicBool,
    limit: u32,
    prefer_disguise: bool,
}
impl ParallelContext {
    fn new(count: u32) -> Self {
        Self {
            count: std::sync::atomic::AtomicU32::new(count),
            disabled: std::sync::atomic::AtomicBool::new(false),
            limit: 16,
            prefer_disguise: false,
        }
    }
}
impl PeerContext for ParallelContext {
    fn network_identity(&self) -> NetworkIdentity {
        NetworkIdentity::default()
    }
    fn parallel_connections(&self) -> u32 {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn disable_parallel_connections(&self) -> bool {
        self.disabled.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn flags(&self) -> Flags {
        Flags {
            default_protocol: "tcp".into(),
            p2p_disguise_mode: if self.prefer_disguise {
                "prefer"
            } else {
                "off"
            }
            .into(),
            ..Default::default()
        }
    }
    fn parallel_connection_limit(&self) -> u32 {
        self.limit
    }
}

/// Deterministic per-connection cap: the receiver consumes one data packet per
/// interval. Control/ping traffic remains independent of the data window.
#[derive(Debug)]
struct CappedTunnel {
    inner: RingTunnel,
    interval: Duration,
}
impl Tunnel for CappedTunnel {
    fn split(&self) -> SplitTunnel {
        let (stream, sink) = self.inner.split();
        let interval = self.interval;
        let stream = stream.then(move |packet| async move {
            if packet.as_ref().is_ok_and(|p| p.peer_manager_header().is_some_and(|h| matches!(h.packet_type, t if t == PacketType::Data as u8 || t == PacketType::ParallelData as u8 || t == PacketType::ParallelLossyData as u8))) {
                tokio::time::sleep(interval).await;
            }
            packet
        });
        (Box::pin(stream), sink)
    }
    fn info(&self) -> Option<TunnelInfo> {
        self.inner.info()
    }
}

async fn add_connection(
    a: &Peer,
    b: &Peer,
    context: ArcPeerContext,
    remote_context: ArcPeerContext,
    token: Option<ParallelMemberToken>,
    cap: Duration,
) -> (PeerConnId, PeerConnId) {
    let work = async {
        let (first, second) = create_ring_socket_pair(64);
        let info = Some(TunnelInfo {
            tunnel_type: "tcp".into(),
            ..Default::default()
        });
        let mut local = PeerConn::new_with_peer_id_hint_and_origin(
            1,
            context,
            Box::new(CappedTunnel {
                inner: RingTunnel::new(first, info.clone()),
                interval: cap,
            }),
            Some(2),
            Arc::new(PeerSessionStore::new()),
            PeerConnectionOrigin::Direct,
        );
        let mut remote = PeerConn::new_with_peer_id_hint_and_origin(
            2,
            remote_context,
            Box::new(CappedTunnel {
                inner: RingTunnel::new(second, info),
                interval: cap,
            }),
            Some(1),
            Arc::new(PeerSessionStore::new()),
            PeerConnectionOrigin::Listener,
        );
        let (left, right) = tokio::join!(
            local.do_handshake_as_client(),
            remote.do_handshake_as_server_ext(|_, _| Ok(()))
        );
        left.unwrap();
        right.unwrap();
        let ids = (local.get_conn_id(), remote.get_conn_id());
        a.add_peer_conn(local).await.unwrap();
        b.add_peer_conn(remote).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if a.conns.get(&ids.0).unwrap().get_stats().latency_us > 0
                    && b.conns.get(&ids.1).unwrap().get_stats().latency_us > 0
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        ids
    };
    super::super::parallel::scope_optional(token, work).await
}

async fn make_pool(count: u32, cap: Duration) -> (Arc<Peer>, Arc<Peer>, PacketRecvChanReceiver) {
    let context: ArcPeerContext = Arc::new(ParallelContext::new(count));
    let (left_tx, _left_rx) = create_packet_recv_chan();
    let (right_tx, right_rx) = create_packet_recv_chan();
    let left = Arc::new(Peer::new(2, left_tx, context.clone()));
    let right = Arc::new(Peer::new(1, right_tx, context.clone()));
    for slot in 0..count {
        let reservation = if slot == 0 {
            None
        } else {
            Some(
                left.reserve_parallel_slot()
                    .expect("pool needs another slot"),
            )
        };
        add_connection(
            &left,
            &right,
            context.clone(),
            context.clone(),
            reservation.as_ref().map(|r| r.token()),
            cap,
        )
        .await;
    }
    assert_eq!(
        left.parallel_connection_status().active_conn_ids.len(),
        count as usize
    );
    assert_eq!(
        right.parallel_connection_status().active_conn_ids.len(),
        count as usize
    );
    (left, right, right_rx)
}

async fn transfer_flow(
    peer: &Peer,
    receiver: &mut PacketRecvChanReceiver,
    packets: u32,
) -> Duration {
    let before = Instant::now();
    let send = async {
        for seq in 0..packets {
            let mut payload = [0u8; 128];
            payload[..4].copy_from_slice(&seq.to_be_bytes());
            let mut packet = ZCPacket::new_with_payload(&payload);
            packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
            peer.send_msg(packet).await.unwrap();
        }
    };
    let recv = async {
        for seq in 0..packets {
            let packet = receiver.recv().await.unwrap();
            assert_eq!(
                &packet.payload()[..4],
                &seq.to_be_bytes(),
                "one flow must retain packet order across connections"
            );
        }
    };
    tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(send, recv);
    })
    .await
    .unwrap();
    before.elapsed()
}

#[tokio::test]
async fn single_flow_uses_every_member_and_scales_with_per_connection_cap() {
    let cap = Duration::from_millis(7);
    let (single, _remote, mut single_rx) = make_pool(1, cap).await;
    let single_duration = transfer_flow(&single, &mut single_rx, 192).await;
    let (pool, _remote_pool, mut pool_rx) = make_pool(4, cap).await;
    let before: HashMap<_, _> = pool
        .conns
        .iter()
        .map(|c| (*c.key(), c.value().get_stats().tx_bytes))
        .collect();
    let pool_duration = transfer_flow(&pool, &mut pool_rx, 192).await;
    eprintln!(
        "single_flow_per_connection_cap: single={single_duration:?}, pool={pool_duration:?}, ratio={:.3}",
        single_duration.as_secs_f64() / pool_duration.as_secs_f64()
    );
    for c in pool.conns.iter() {
        assert!(
            c.value().get_stats().tx_bytes > before[c.key()],
            "each independent connection must send data"
        );
    }
    assert!(
        single_duration.as_secs_f64() / pool_duration.as_secs_f64() >= 2.5,
        "single={single_duration:?}, pool={pool_duration:?}"
    );
}

#[tokio::test]
async fn remaining_members_keep_sequence_and_replacement_reuses_vacant_slot() {
    let (left, right, mut receiver) = make_pool(4, Duration::from_millis(1)).await;
    transfer_flow(&left, &mut receiver, 8).await;
    let removed = left
        .conns
        .iter()
        .find(|c| c.value().parallel_member().is_some_and(|m| m.slot == 2))
        .unwrap()
        .get_conn_id();
    left.close_peer_conn(&removed).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while left.conns.contains_key(&removed) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    transfer_flow(&left, &mut receiver, 8).await;
    let reservation = left
        .reserve_parallel_slot()
        .expect("vacant slot is refillable");
    assert_eq!(reservation.token().slot, 2);
    let context: ArcPeerContext = Arc::new(ParallelContext::new(4));
    add_connection(
        &left,
        &right,
        context.clone(),
        context,
        Some(reservation.token()),
        Duration::from_millis(1),
    )
    .await;
    transfer_flow(&left, &mut receiver, 8).await;
    assert_eq!(left.parallel_connection_status().active_conn_ids.len(), 4);
}

#[tokio::test]
async fn runtime_disable_acknowledges_drains_and_can_reenable() {
    let left_context = Arc::new(ParallelContext::new(4));
    let right_context = Arc::new(ParallelContext::new(4));
    let (left_tx, _left_rx) = create_packet_recv_chan();
    let (right_tx, mut receiver) = create_packet_recv_chan();
    let left = Peer::new(2, left_tx, left_context.clone());
    let right = Peer::new(1, right_tx, right_context.clone());
    for slot in 0..4 {
        let reservation = if slot == 0 {
            None
        } else {
            Some(left.reserve_parallel_slot().unwrap())
        };
        add_connection(
            &left,
            &right,
            left_context.clone(),
            right_context.clone(),
            reservation.as_ref().map(|r| r.token()),
            Duration::from_millis(1),
        )
        .await;
    }
    transfer_flow(&left, &mut receiver, 16).await;
    left_context
        .disabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if left.parallel_connection_status().target == 1
                && right.parallel_connection_status().target == 1
                && left.conns.len() == 1
                && right.conns.len() == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both peers should acknowledge the disabled offer and retire extras");
    transfer_flow(&left, &mut receiver, 16).await;
    left_context
        .disabled
        .store(false, std::sync::atomic::Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(5), async {
        while right.parallel_connection_status().target != 4 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..3 {
        let reservation = left.reserve_parallel_slot().unwrap();
        add_connection(
            &left,
            &right,
            left_context.clone(),
            right_context.clone(),
            Some(reservation.token()),
            Duration::from_millis(1),
        )
        .await;
    }
    transfer_flow(&left, &mut receiver, 16).await;
    assert_eq!(left.parallel_connection_status().active_conn_ids.len(), 4);
    assert_eq!(right.parallel_connection_status().active_conn_ids.len(), 4);
}

#[tokio::test]
async fn new_bootstrap_authorizes_a_fresh_epoch_while_old_remote_connections_remain() {
    let (old_left, right, mut receiver) = make_pool(4, Duration::from_millis(1)).await;
    transfer_flow(&old_left, &mut receiver, 8).await;
    let context: ArcPeerContext = Arc::new(ParallelContext::new(4));
    let (tx, _rx) = create_packet_recv_chan();
    let restarted_left = Peer::new(2, tx, context.clone());
    add_connection(
        &restarted_left,
        &right,
        context.clone(),
        context,
        None,
        Duration::from_millis(1),
    )
    .await;
    assert!(
        right.conns.len() > 1,
        "the remote has not observed an empty pool"
    );
    transfer_flow(&restarted_left, &mut receiver, 8).await;
}

#[tokio::test]
async fn capped_pool_releases_one_old_member_before_protocol_migration() {
    for cap in [3, 16] {
        let mut context = ParallelContext::new(16);
        context.limit = cap;
        context.prefer_disguise = true;
        let context: ArcPeerContext = Arc::new(context);
        let (left_tx, _left_rx) = create_packet_recv_chan();
        let (right_tx, _right_rx) = create_packet_recv_chan();
        let left = Peer::new(2, left_tx, context.clone());
        let right = Peer::new(1, right_tx, context.clone());
        for slot in 0..cap {
            let reservation = if slot == 0 {
                None
            } else {
                Some(left.reserve_parallel_slot().unwrap())
            };
            add_connection(
                &left,
                &right,
                context.clone(),
                context.clone(),
                reservation.as_ref().map(|r| r.token()),
                Duration::from_millis(1),
            )
            .await;
        }
        assert_eq!(left.parallel_connection_status().target, cap);
        assert!(
            left.reserve_parallel_slot_for_protocol(Some("http3"))
                .is_none()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while left.conns.len() != cap as usize - 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            left.conns
                .iter()
                .all(|conn| conn.get_stats().latency_us > 0)
        );
        let reservation = left
            .reserve_parallel_slot_for_protocol(Some("http3"))
            .expect("one quota slot is now free for the new protocol");
        assert_eq!(reservation.token().slot, 0);
        assert_eq!(left.conns.len(), cap as usize - 1);
    }
}
