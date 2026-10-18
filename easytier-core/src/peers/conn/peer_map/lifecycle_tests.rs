use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};

use futures::poll;
use tokio::sync::Notify;

use super::PeerMap;
use crate::{
    packet::{PacketType, ZCPacket},
    peers::{
        PacketRecvChanReceiver, PeerConnectionOrigin,
        conn::{peer_conn::PeerConn, peer_session::PeerSessionStore},
        context::{ArcPeerContext, NetworkIdentity, PeerContext, PeerEvent},
        create_packet_recv_chan,
    },
    proto::common::{Flags, TunnelInfo},
    tunnel::ring::{RingTunnel, create_ring_socket_pair},
};

#[derive(Clone, Copy)]
enum PausedEvent {
    Added,
    Removed,
}

struct EventGate {
    event: PausedEvent,
    armed: AtomicBool,
    entered: Notify,
    released: Mutex<bool>,
    release: Condvar,
}

impl EventGate {
    fn new(event: PausedEvent) -> Arc<Self> {
        Arc::new(Self {
            event,
            armed: AtomicBool::new(false),
            entered: Notify::new(),
            released: Mutex::new(false),
            release: Condvar::new(),
        })
    }

    fn pause(&self, event: &PeerEvent) {
        let matches = matches!(
            (self.event, event),
            (PausedEvent::Added, PeerEvent::PeerConnAdded(_))
                | (PausedEvent::Removed, PeerEvent::PeerConnRemoved(_))
        );
        if matches && self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.release.wait(released).unwrap();
            }
        }
    }

    async fn wait_entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("connection event should reach its controlled pause");
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

// Unwind must release the synchronous context callback before Tokio shuts down.
struct ReleaseGate(Arc<EventGate>);

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct ParallelContext {
    gate: Option<Arc<EventGate>>,
}

impl PeerContext for ParallelContext {
    fn network_identity(&self) -> NetworkIdentity {
        NetworkIdentity::default()
    }

    fn parallel_connections(&self) -> u32 {
        2
    }

    fn flags(&self) -> Flags {
        Flags {
            default_protocol: "tcp".into(),
            ..Default::default()
        }
    }

    fn issue_event(&self, event: PeerEvent) {
        if let Some(gate) = self.gate.as_ref() {
            gate.pause(&event);
        }
    }
}

async fn handshaken_pair(
    left_context: ArcPeerContext,
    right_context: ArcPeerContext,
) -> (PeerConn, PeerConn) {
    let (left_socket, right_socket) = create_ring_socket_pair(64);
    let info = Some(TunnelInfo {
        tunnel_type: "tcp".into(),
        ..Default::default()
    });
    let sessions = Arc::new(PeerSessionStore::new());
    let mut left = PeerConn::new_with_peer_id_hint_and_origin(
        1,
        left_context,
        Box::new(RingTunnel::new(left_socket, info.clone())),
        Some(2),
        sessions.clone(),
        PeerConnectionOrigin::Direct,
    );
    let mut right = PeerConn::new_with_peer_id_hint_and_origin(
        2,
        right_context,
        Box::new(RingTunnel::new(right_socket, info)),
        Some(1),
        sessions,
        PeerConnectionOrigin::Listener,
    );
    let (left_result, right_result) = tokio::join!(
        left.do_handshake_as_client(),
        right.do_handshake_as_server_ext(|_, _| Ok(())),
    );
    left_result.unwrap();
    right_result.unwrap();
    (left, right)
}

async fn wait_for_ping(map: &PeerMap, remote: u32, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(conns) = map.list_peer_conns(remote).await
                && conns
                    .iter()
                    .filter(|c| !c.is_closed && c.stats.as_ref().is_some_and(|s| s.latency_us > 0))
                    .count()
                    == count
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all retained connections should answer ping");
}

async fn deliver(
    map: &PeerMap,
    remote: u32,
    receiver: &mut PacketRecvChanReceiver,
    payload: &[u8],
) {
    let mut packet = ZCPacket::new_with_payload(payload);
    packet.fill_peer_manager_hdr(map.my_peer_id(), remote, PacketType::Data as u8);
    map.send_msg_directly(packet, remote).await.unwrap();
    let received = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("authenticated live connections must accept the peer's transmit epoch")
        .unwrap();
    assert_eq!(received.payload(), payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_first_admissions_preserve_connections_and_receive_epochs() {
    let gate = EventGate::new(PausedEvent::Added);
    let release = ReleaseGate(gate.clone());
    let left_context: ArcPeerContext = Arc::new(ParallelContext {
        gate: Some(gate.clone()),
    });
    let right_context: ArcPeerContext = Arc::new(ParallelContext { gate: None });
    let (left_sender, mut left_receiver) = create_packet_recv_chan();
    let (right_sender, mut right_receiver) = create_packet_recv_chan();
    let left = Arc::new(PeerMap::new(left_sender, left_context.clone(), 1));
    let right = PeerMap::new(right_sender, right_context.clone(), 2);
    let (first_left, first_right) =
        handshaken_pair(left_context.clone(), right_context.clone()).await;
    let (second_left, second_right) = handshaken_pair(left_context, right_context).await;

    // The remote keeps the second connection's transmit epoch. If two local
    // Peer objects are published, the first admission overwrites the second
    // and loses authorization for precisely that still-live remote epoch.
    right.add_new_peer_conn(second_right).await.unwrap();
    right.add_new_peer_conn(first_right).await.unwrap();
    gate.armed.store(true, Ordering::SeqCst);
    let first_map = left.clone();
    let first = tokio::spawn(async move { first_map.add_new_peer_conn(first_left).await });
    gate.wait_entered().await;

    let mut second = Box::pin(left.add_new_peer_conn(second_left));
    let second_result = match poll!(second.as_mut()) {
        Poll::Ready(result) => Some(result),
        Poll::Pending => None,
    };
    drop(release);
    first.await.unwrap().unwrap();
    match second_result {
        Some(result) => result.unwrap(),
        None => second.await.unwrap(),
    }

    // Exercise actual striped data before checking membership: an overwritten
    // Peer can still have a healthy surviving connection while rejecting data.
    wait_for_ping(&left, 2, left.list_peer_conns(2).await.unwrap().len()).await;
    deliver(&right, 1, &mut left_receiver, b"remote bootstrap epoch").await;
    deliver(&left, 2, &mut right_receiver, b"local bootstrap epoch").await;
    assert_eq!(left.list_peer_conns(2).await.unwrap().len(), 2);
    assert_eq!(right.list_peer_conns(1).await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_during_last_connection_removal_starts_a_fresh_epoch() {
    let gate = EventGate::new(PausedEvent::Removed);
    let release = ReleaseGate(gate.clone());
    let left_context: ArcPeerContext = Arc::new(ParallelContext {
        gate: Some(gate.clone()),
    });
    let right_context: ArcPeerContext = Arc::new(ParallelContext { gate: None });
    let (left_sender, mut left_receiver) = create_packet_recv_chan();
    let (right_sender, _old_receiver) = create_packet_recv_chan();
    let left = PeerMap::new(left_sender, left_context.clone(), 1);
    let old_right = PeerMap::new(right_sender, right_context.clone(), 2);
    let (old_left_conn, old_right_conn) =
        handshaken_pair(left_context.clone(), right_context.clone()).await;
    let old_conn_id = old_left_conn.get_conn_id();
    left.add_new_peer_conn(old_left_conn).await.unwrap();
    old_right.add_new_peer_conn(old_right_conn).await.unwrap();
    gate.armed.store(true, Ordering::SeqCst);
    left.close_peer_conn(2, &old_conn_id).await.unwrap();
    gate.wait_entered().await;
    old_right.close_peer(1).await.unwrap();

    let (right_sender, mut right_receiver) = create_packet_recv_chan();
    let right = PeerMap::new(right_sender, right_context.clone(), 2);
    let (new_left_conn, new_right_conn) = handshaken_pair(left_context, right_context).await;
    right.add_new_peer_conn(new_right_conn).await.unwrap();
    left.add_new_peer_conn(new_left_conn).await.unwrap();
    drop(release);

    wait_for_ping(&left, 2, 1).await;
    wait_for_ping(&right, 1, 1).await;
    deliver(&left, 2, &mut right_receiver, b"fresh local generation").await;
    deliver(&right, 1, &mut left_receiver, b"fresh remote generation").await;
}
