use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use arc_swap::ArcSwapOption;
use crossbeam_utils::atomic::AtomicCell;
use dashmap::{DashMap, DashSet};
use parking_lot::{Mutex, RwLock};

use tokio::{select, sync::mpsc};

use tracing::Instrument;

use super::{
    peer_conn::{PeerConn, PeerConnId},
    peer_flow::FlowBindings,
};
use crate::peers::{
    PacketRecvChan,
    context::{ArcPeerContext, PeerEvent},
    util::shrink_dashmap,
};
use crate::{
    config::PeerId,
    foundation::small_map_shard_amount,
    packet::{FlowKey, ZCPacket},
    peers::error::Error,
    proto::{core_peer::peer::PeerConnInfo, peer_rpc::PeerIdentityType},
};
use tokio_util::task::AbortOnDropHandle;

#[cfg(test)]
use crate::proto::common::Flags;
#[cfg(test)]
use std::time::Duration;

mod scheduling;

#[cfg(test)]
use scheduling::{LaneModel, conn_latency_sort_key, preferred_conn_sort_key};
use scheduling::{
    ParallelSendState, PeerConnCloseEvent, SendPlanCache, reconcile_parallel_anchor,
    reconcile_peer_connections, select_preferred_conn,
};

type ArcPeerConn = Arc<PeerConn>;
type ConnMap = Arc<DashMap<PeerConnId, ArcPeerConn>>;

pub struct Peer {
    pub peer_node_id: PeerId,
    conns: ConnMap,
    context: ArcPeerContext,

    packet_recv_chan: PacketRecvChan,

    close_event_sender: mpsc::Sender<PeerConnCloseEvent>,
    #[allow(dead_code)]
    close_event_listener: AbortOnDropHandle<()>,
    conn_lifecycle: Arc<tokio::sync::Mutex<()>>,

    shutdown_notifier: Arc<tokio::sync::Notify>,

    default_conn: Arc<ArcSwapOption<PeerConn>>,
    default_conn_update_lock: Arc<Mutex<()>>,
    negotiated_disguise: Arc<AtomicCell<Option<bool>>>,
    remote_cleanup_requested: Arc<AtomicCell<bool>>,
    flow_bindings: Mutex<FlowBindings>,
    data_active: Arc<AtomicBool>,
    peer_identity_type: Arc<AtomicCell<Option<PeerIdentityType>>>,
    peer_public_key: Arc<RwLock<Option<Vec<u8>>>>,
    #[allow(dead_code)]
    conn_maintenance_task: AbortOnDropHandle<()>,
    parallel_send: Arc<Mutex<ParallelSendState>>,
    send_plan: Arc<SendPlanCache>,
    parallel_reservations: Arc<super::parallel::ParallelReservations>,
    parallel_receiver: Arc<super::parallel::ParallelReceiver>,
    parallel_policy: Arc<super::parallel::ParallelPolicy>,
    #[allow(dead_code)]
    parallel_receive_task: AbortOnDropHandle<()>,
}

impl Peer {
    pub(crate) fn new(
        peer_node_id: PeerId,
        packet_recv_chan: PacketRecvChan,
        context: ArcPeerContext,
    ) -> Self {
        let conns: ConnMap = Arc::new(DashMap::with_shard_amount(small_map_shard_amount()));
        let (close_event_sender, mut close_event_receiver) =
            mpsc::channel::<PeerConnCloseEvent>(10);
        let shutdown_notifier = Arc::new(tokio::sync::Notify::new());
        let peer_identity_type = Arc::new(AtomicCell::new(None));
        let peer_identity_type_copy = peer_identity_type.clone();
        let peer_public_key = Arc::new(RwLock::new(None));
        let peer_public_key_copy = peer_public_key.clone();
        let default_conn = Arc::new(ArcSwapOption::empty());
        let default_conn_update_lock = Arc::new(Mutex::new(()));
        let negotiated_disguise = Arc::new(AtomicCell::new(None));
        let remote_cleanup_requested = Arc::new(AtomicCell::new(false));
        let parallel_send = Arc::new(Mutex::new(ParallelSendState::default()));
        let send_plan = Arc::new(SendPlanCache::default());
        let parallel_receiver = super::parallel::ParallelReceiver::new(packet_recv_chan.clone());
        let parallel_policy = Arc::new(super::parallel::ParallelPolicy::default());
        let conn_lifecycle = Arc::new(tokio::sync::Mutex::new(()));
        let receiver = parallel_receiver.clone();
        let receive_conns = conns.clone();
        let parallel_receive_task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                receiver.wait_buffered().await;
                crate::foundation::time::sleep(std::time::Duration::from_millis(10)).await;
                receiver.set_max_rtt_us(
                    receive_conns
                        .iter()
                        .filter(|c| !c.value().is_closed())
                        .map(|c| c.value().get_stats().latency_us)
                        .max()
                        .unwrap_or(0),
                );
                receiver.flush_expired().await;
            }
        }));
        let parallel_send_copy = parallel_send.clone();
        let parallel_receiver_copy = parallel_receiver.clone();
        let parallel_policy_copy = parallel_policy.clone();
        let data_active = Arc::new(AtomicBool::new(false));
        let flow_bindings = Mutex::new(FlowBindings::default());

        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let context_copy = context.clone();
        let default_conn_copy = default_conn.clone();
        let default_conn_update_lock_copy = default_conn_update_lock.clone();
        let negotiated_disguise_copy = negotiated_disguise.clone();
        let remote_cleanup_requested_copy = remote_cleanup_requested.clone();
        let conn_lifecycle_copy = conn_lifecycle.clone();
        let send_plan_copy = send_plan.clone();
        let close_event_listener = AbortOnDropHandle::new(tokio::spawn(
            async move {
                loop {
                    select! {
                        ret = close_event_receiver.recv() => {
                            if ret.is_none() {
                                break;
                            }
                            let ret = ret.unwrap();
                            tracing::warn!(
                                ?peer_node_id,
                                ?ret,
                                "notified that peer conn is closed",
                            );

                            let removed_conn = {
                                let _lifecycle = conn_lifecycle_copy.lock().await;
                                let _update_guard = default_conn_update_lock_copy.lock();
                                // Cleanup is conditional until the actual removal: a newer
                                // preference or failed replacement must retain the fallback.
                                let Some(conn_id) = ret.removable_id(&conns_copy, &context_copy.flags(), negotiated_disguise_copy.load()) else { continue };
                                let removed_conn = conns_copy.remove(&conn_id);
                                if let Some((_, conn)) = removed_conn.as_ref() {
                                    send_plan_copy.invalidate();
                                    parallel_receiver_copy.forget_lane(&conn_id);
                                    let cached_conn = default_conn_copy.load();
                                    if cached_conn
                                        .as_ref()
                                        .is_some_and(|cached| Arc::ptr_eq(cached, conn))
                                    {
                                        default_conn_copy.store(None);
                                    }
                                    // Reset before admitting another connection. Deferring
                                    // this until after callbacks can skip the reset when
                                    // a new bootstrap fills the temporarily empty map.
                                    if conns_copy.is_empty() {
                                        *parallel_send_copy.lock() = ParallelSendState::default();
                                        parallel_receiver_copy.reset_epoch();
                                        parallel_policy_copy.reset();
                                        remote_cleanup_requested_copy.store(false);
                                        peer_identity_type_copy.store(None);
                                        *peer_public_key_copy.write() = None;
                                    }
                                }
                                removed_conn
                            };

                            if let Some((_, conn)) = removed_conn {
                                context_copy.issue_event(PeerEvent::PeerConnRemoved(
                                    conn.get_conn_info(),
                                ));
                                shrink_dashmap(&conns_copy, Some(4));
                            }
                        }

                        _ = shutdown_notifier_copy.notified() => {
                            close_event_receiver.close();
                            tracing::warn!(?peer_node_id, "peer close event listener notified");
                        }
                    }
                }
                tracing::info!("peer {} close event listener exit", peer_node_id);
            }
            .instrument(tracing::info_span!(
                "peer_close_event_listener",
                ?peer_node_id,
            )),
        ));

        let conns_copy = conns.clone();
        let default_conn_copy = default_conn.clone();
        let context_copy = context.clone();
        let default_conn_update_lock_copy = default_conn_update_lock.clone();
        let close_event_sender_copy = close_event_sender.clone();
        let negotiated_disguise_copy = negotiated_disguise.clone();
        let parallel_policy_copy = parallel_policy.clone();
        let parallel_send_maintenance = parallel_send.clone();
        let send_plan_copy = send_plan.clone();
        let data_active_copy = data_active.clone();
        let conn_maintenance_task = AbortOnDropHandle::new(tokio::spawn(async move {
            // Fast ping follows data activity over five-second windows.
            let mut ticks = 0u32;
            loop {
                crate::foundation::time::sleep(std::time::Duration::from_millis(250)).await;
                send_plan_copy.invalidate();
                reconcile_parallel_anchor(
                    &conns_copy,
                    &parallel_send_maintenance,
                    &parallel_policy_copy,
                    peer_node_id,
                    &close_event_sender_copy,
                )
                .await;
                if let Some(conn) = select_preferred_conn(&conns_copy, &context_copy.flags())
                    && super::parallel::remote_offer(&conn.get_conn_info().features).is_some()
                {
                    let payload = parallel_policy_copy.control_payload(
                        conn.local_parallel_offer(),
                        parallel_send_maintenance.lock().pool_anchor,
                    );
                    let mut packet = ZCPacket::new_with_payload(&payload);
                    let info = conn.get_conn_info();
                    packet.fill_peer_manager_hdr(
                        info.my_peer_id,
                        peer_node_id,
                        crate::packet::PacketType::ParallelControl as u8,
                    );
                    let bytes = packet.buf_len() as u64;
                    if conn.send_msg(packet).await.is_ok() {
                        context_copy.record_control_tx(&info.network_name, bytes);
                    }
                }
                if conns_copy.len() > 1 {
                    reconcile_peer_connections(
                        &conns_copy,
                        &context_copy.flags(),
                        &negotiated_disguise_copy,
                        &default_conn_copy,
                        &default_conn_update_lock_copy,
                        &close_event_sender_copy,
                        peer_node_id,
                    )
                    .await;
                }
                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(20) {
                    let active = data_active_copy.swap(false, Ordering::Relaxed);
                    let fast_ping = conns_copy.len() > 1 && active;
                    for conn in conns_copy.iter() {
                        conn.fast_ping.store(fast_ping, Ordering::Relaxed);
                    }
                }
            }
        }));

        Peer {
            peer_node_id,
            conns,
            packet_recv_chan,
            context,

            close_event_sender,
            close_event_listener,
            conn_lifecycle,

            shutdown_notifier,
            default_conn,
            default_conn_update_lock,
            negotiated_disguise,
            remote_cleanup_requested,
            flow_bindings,
            data_active,
            peer_identity_type,
            peer_public_key,
            conn_maintenance_task,
            parallel_send,
            send_plan,
            parallel_reservations: Arc::new(super::parallel::ParallelReservations::default()),
            parallel_receiver,
            parallel_policy,
            parallel_receive_task,
        }
    }

    pub async fn add_peer_conn(&self, mut conn: PeerConn) -> Result<(), Error> {
        let lifecycle = self.conn_lifecycle.lock().await;
        if !self.conns.is_empty() && self.conns.iter().all(|c| c.value().is_closed()) {
            *self.parallel_send.lock() = ParallelSendState::default();
            self.parallel_receiver.reset_epoch();
            self.parallel_policy.reset();
        }
        let conn_identity_type = conn.get_peer_identity_type();
        let peer_identity_type = self.peer_identity_type.load();
        if let Some(peer_identity_type) = peer_identity_type {
            if peer_identity_type != conn_identity_type {
                return Err(Error::SecretKeyError(format!(
                    "peer identity type mismatch. peer: {:?}, conn: {:?}",
                    peer_identity_type, conn_identity_type
                )));
            }
        } else {
            self.peer_identity_type.store(Some(conn_identity_type));
        }

        let close_notifier = conn.get_close_notifier();
        let conn_info = conn.get_conn_info();
        let conn_pubkey = conn_info.noise_remote_static_pubkey.clone();
        {
            let mut peer_pubkey = self.peer_public_key.write();
            if let Some(existing_pubkey) = peer_pubkey.as_ref() {
                if existing_pubkey != &conn_pubkey {
                    return Err(Error::SecretKeyError(format!(
                        "peer public key mismatch. peer_id: {}, existing_len: {}, new_len: {}",
                        self.peer_node_id,
                        existing_pubkey.len(),
                        conn_pubkey.len()
                    )));
                }
            } else {
                *peer_pubkey = Some(conn_pubkey);
            }
        }

        let mut obsolete_members = Vec::new();
        if super::parallel::remote_offer(&conn_info.features).is_some()
            && let Some(remote_epoch) = super::parallel::tx_epoch(&conn_info.features)
        {
            let mut state = self.parallel_send.lock();
            if let Some(member) = conn.parallel_member() {
                if member.slot as u32 >= conn.parallel_target()
                    || state.pool_anchor != Some((member.pool_id, member.epoch))
                {
                    return Err(Error::WaitRespError(
                        "parallel pool epoch or slot does not match the live peer".into(),
                    ));
                }
            } else {
                // Restart the sequence under the epoch the remote authorizes in
                // this handshake. The remote may have dropped every connection
                // while ours stayed open; its reset receiver accepts no other
                // epoch, so keeping the old one would silently lose all data.
                state.epoch = conn.parallel_tx_epoch();
                state.sequence = 0;
                if let Some(anchor) = conn.cleanup_policy().shared_connection_key()
                    && state.pool_anchor.is_none_or(|previous| anchor < previous)
                {
                    state.pool_anchor = Some(anchor);
                    obsolete_members = self
                        .conns
                        .iter()
                        .filter(|entry| {
                            entry
                                .value()
                                .parallel_member()
                                .is_some_and(|member| (member.pool_id, member.epoch) != anchor)
                        })
                        .map(|entry| *entry.key())
                        .collect();
                }
                self.parallel_receiver.authorize_epoch(remote_epoch);
            }
        }

        // A peer may enable cleanup after the surviving manual connection's
        // handshake. Use the latest supported offer (including opt-out), so
        // retired URLs neither churn nor remain suppressed by an old offer.
        if let Some(enabled) = conn.cleanup_policy().remote_cleanup_enabled() {
            self.remote_cleanup_requested.store(enabled);
        }
        conn.set_parallel_receiver(self.parallel_receiver.clone());
        conn.set_parallel_policy(self.parallel_policy.clone());
        conn.start_recv_loop(self.packet_recv_chan.clone()).await;
        conn.start_pingpong();
        self.conns.insert(conn.get_conn_id(), Arc::new(conn));
        self.send_plan.invalidate();
        // Close queue backpressure, callbacks and reconciliation must run after
        // releasing this guard: the close listener needs it to drain the queue.
        drop(lifecycle);
        for id in obsolete_members {
            let _ = self
                .close_event_sender
                .send(PeerConnCloseEvent::Closed(id))
                .await;
        }

        let close_event_sender = self.close_event_sender.clone();
        tokio::spawn(async move {
            let conn_id = close_notifier.get_conn_id();
            if let Some(mut waiter) = close_notifier.get_waiter().await {
                let _ = waiter.recv().await;
            }
            if let Err(e) = close_event_sender
                .send(PeerConnCloseEvent::Closed(conn_id))
                .await
            {
                tracing::warn!(?conn_id, "failed to send close event: {}", e);
            }
        });

        self.context
            .issue_event(PeerEvent::PeerConnAdded(conn_info));

        self.reconcile_connections().await;
        Ok(())
    }

    async fn reconcile_connections(&self) {
        reconcile_peer_connections(
            &self.conns,
            &self.context.flags(),
            &self.negotiated_disguise,
            &self.default_conn,
            &self.default_conn_update_lock,
            &self.close_event_sender,
            self.peer_node_id,
        )
        .await;
    }

    fn select_conn(&self) -> Option<ArcPeerConn> {
        let _update_guard = self.default_conn_update_lock.lock();
        if let Some(conn) = self.default_conn.load_full()
            && !conn.is_closed()
        {
            return Some(conn);
        }

        // A zero latency on a hole-punched connection means the ping loop has not
        // confirmed liveness yet. Prefer any other connection, so a freshly admitted
        // hole-punched path cannot steal traffic before its first successful ping.
        let flags = self.context.flags();
        let selected = select_preferred_conn(&self.conns, &flags);

        if let Some(conn) = selected.as_ref() {
            self.default_conn.store(Some(conn.clone()));
        }
        selected
    }

    fn select_flow_conn(&self, key: FlowKey) -> Option<ArcPeerConn> {
        let mut flows = self.flow_bindings.lock();
        let bucket = flows.bucket(key);
        if let Some(binding) = flows.buckets.get_mut(&bucket)
            && binding.last_active.elapsed() < FlowBindings::IDLE_TIMEOUT
        {
            let bound = self
                .conns
                .get(&binding.conn_id)
                .map(|conn| conn.value().clone());
            if let Some(conn) = bound.filter(|conn| !conn.is_closed()) {
                binding.last_active = Instant::now();
                return Some(conn);
            }
        }
        let conn = self.select_conn()?;
        // A collision joins the bucket's active binding rather than evicting an
        // existing flow. This bounds memory without migrating flows under load.
        flows.bind(bucket, conn.get_conn_id(), Instant::now());
        Some(conn)
    }

    pub async fn send_msg(&self, msg: ZCPacket) -> Result<(), Error> {
        match self.try_send_msg(msg).await? {
            None => Ok(()),
            Some(_) => Err(Error::PeerNoConnectionError(self.peer_node_id)),
        }
    }

    /// Returns the unsent packet only when no usable connection can be selected.
    pub(crate) async fn try_send_msg(&self, msg: ZCPacket) -> Result<Option<ZCPacket>, Error> {
        if crate::peers::traffic_metrics::data_packet_payload_len(&msg).is_some() {
            self.data_active.store(true, Ordering::Relaxed);
        }
        if super::parallel::stripeable(&msg) {
            let mut plan = self.send_plan();
            if plan.target > 1 && plan.members.is_empty() {
                let ready = tokio::time::timeout(std::time::Duration::from_secs(7), async {
                    loop {
                        if self.conns.iter().all(|conn| conn.value().is_closed()) {
                            return None;
                        }
                        self.send_plan.invalidate();
                        let plan = self.send_plan();
                        if plan.target == 1 || !plan.members.is_empty() {
                            return Some(plan);
                        }
                        crate::foundation::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                })
                .await;
                let Ok(Some(ready)) = ready else {
                    return Err(Error::PeerNoConnectionError(self.peer_node_id));
                };
                plan = ready;
            }
            // Striping orders every packet on the receiver, so flows need no
            // connection binding while it runs.
            if plan.stripe {
                return self.send_striped(msg, &plan).await.map(|()| None);
            }
        }
        if let Some(key) = msg.flow_key {
            let Some(conn) = self.select_flow_conn(key) else {
                return Ok(Some(msg));
            };
            conn.send_msg(msg).await?;
            return Ok(None);
        }
        let default_conn = self.default_conn.load();
        if let Some(conn) = default_conn.as_ref()
            && !conn.is_closed()
        {
            conn.send_msg(msg).await?;
            return Ok(None);
        }
        drop(default_conn);

        let Some(conn) = self.select_conn() else {
            return Ok(Some(msg));
        };
        conn.send_msg(msg).await?;

        Ok(None)
    }

    pub async fn close_peer_conn(&self, conn_id: &PeerConnId) -> Result<(), Error> {
        let has_key = self.conns.contains_key(conn_id);
        if !has_key {
            return Err(Error::NotFound);
        }
        self.close_event_sender
            .send(PeerConnCloseEvent::Closed(*conn_id))
            .await
            .unwrap();
        Ok(())
    }

    pub async fn list_peer_conns(&self) -> Vec<PeerConnInfo> {
        let mut conns = vec![];
        for conn in self.conns.iter() {
            // do not lock here, otherwise it will cause dashmap deadlock
            conns.push(conn.clone());
        }

        let mut ret = Vec::new();
        for conn in conns {
            let info = conn.get_conn_info();
            if !info.is_closed {
                ret.push(info);
            } else {
                let conn_id = info.conn_id.parse().unwrap();
                let _ = self.close_peer_conn(&conn_id).await;
            }
        }
        ret
    }

    pub fn has_live_conns(&self) -> bool {
        if self
            .default_conn
            .load()
            .as_ref()
            .is_some_and(|conn| !conn.is_closed())
        {
            return true;
        }
        self.conns.iter().any(|entry| !entry.value().is_closed())
    }

    pub(crate) fn has_usable_manual_connection(&self) -> bool {
        let flags = self.context.flags();
        self.conns.iter().any(|entry| {
            let conn = entry.value();
            conn.is_manual()
                && !conn.is_closed()
                && conn.get_stats().latency_us > 0
                && conn.cleanup_policy().matches_current_flags(&flags)
                && (flags.close_redundant_conns_when_disguised
                    || self.remote_cleanup_requested.load())
        })
    }

    pub fn has_directly_connected_conn(&self) -> bool {
        self.conns
            .iter()
            .any(|entry| !entry.value().is_closed() && !entry.value().is_hole_punched())
    }

    pub(crate) fn has_direct_attached_conn(&self) -> bool {
        self.conns.iter().any(|entry| {
            let conn = entry.value();
            !conn.is_closed() && !conn.is_hole_punched() && conn.is_attached()
        })
    }

    pub fn get_directly_connections(&self) -> DashSet<uuid::Uuid> {
        self.conns
            .iter()
            .filter(|entry| !(entry.value()).is_hole_punched())
            .map(|entry| (entry.value()).get_conn_id())
            .collect()
    }

    pub fn get_default_conn_id(&self) -> PeerConnId {
        self.default_conn
            .load()
            .as_ref()
            .map(|conn| conn.get_conn_id())
            .unwrap_or_default()
    }

    pub fn get_peer_identity_type(&self) -> Option<PeerIdentityType> {
        self.peer_identity_type.load()
    }

    pub fn get_peer_public_key(&self) -> Option<Vec<u8>> {
        self.peer_public_key.read().clone()
    }
}

// pritn on drop
impl Drop for Peer {
    fn drop(&mut self) {
        self.conns.retain(|_, conn| {
            self.context
                .issue_event(PeerEvent::PeerConnRemoved(conn.get_conn_info()));
            false
        });
        self.shutdown_notifier.notify_one();
        tracing::info!("peer {} drop", self.peer_node_id);
    }
}

#[cfg(test)]
mod custom_tests;
#[cfg(test)]
mod parallel_tests;

#[cfg(test)]
mod tests {
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
    async fn cached_live_connection_does_not_scan_connection_map() {
        let peer = Arc::new(new_peer());
        assert!(!peer.has_live_conns());
        let (sender, _receiver) = create_packet_recv_chan();
        let (conn_id, _server) = add_connection(&peer, sender, Some(1_000)).await;
        assert!(peer.has_live_conns());
        peer.select_conn().unwrap();

        // Cached availability must not wait for the connection map's shard lock.
        let conn_guard = peer.conns.get_mut(&conn_id).unwrap();
        let check_peer = peer.clone();
        let check = tokio::task::spawn_blocking(move || check_peer.has_live_conns());
        let result = crate::foundation::time::timeout(Duration::from_secs(1), check).await;
        drop(conn_guard);
        assert!(result.unwrap().unwrap());
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
        assert!(peer.has_live_conns());
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
}
