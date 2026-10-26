use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwapOption;
use crossbeam_utils::atomic::AtomicCell;
use dashmap::{DashMap, DashSet};
use parking_lot::{Mutex, RwLock};

use tokio::{select, sync::mpsc};

use tracing::Instrument;

use super::{
    cleanup_policy::CleanupPolicyPair,
    peer_conn::{PeerConn, PeerConnId},
};
use crate::peers::{
    PacketRecvChan,
    context::{ArcPeerContext, PeerEvent},
    util::shrink_dashmap,
};
use crate::{
    config::{PeerId, p2p_protocol_rank},
    foundation::small_map_shard_amount,
    packet::ZCPacket,
    peers::error::Error,
    proto::{common::Flags, core_peer::peer::PeerConnInfo, peer_rpc::PeerIdentityType},
};
use tokio_util::task::AbortOnDropHandle;

type ArcPeerConn = Arc<PeerConn>;
type ConnMap = Arc<DashMap<PeerConnId, ArcPeerConn>>;

struct ParallelSendState {
    epoch: uuid::Uuid,
    sequence: u64,
    sent_bytes: HashMap<PeerConnId, u64>,
    pool_anchor: Option<(uuid::Uuid, uuid::Uuid)>,
}
impl Default for ParallelSendState {
    fn default() -> Self {
        Self {
            epoch: uuid::Uuid::new_v4(),
            sequence: 0,
            sent_bytes: HashMap::new(),
            pool_anchor: None,
        }
    }
}

fn parallel_members(conns: &ConnMap, flags: &Flags) -> (u32, Vec<ArcPeerConn>) {
    let Some(preferred) = select_preferred_conn(conns, flags) else {
        return (1, Vec::new());
    };
    let target = preferred.parallel_target();
    let preferred_info = preferred.get_conn_info();
    let Some(tunnel) = preferred_info.tunnel.as_ref() else {
        return (1, Vec::new());
    };
    let protocol = tunnel
        .tunnel_type
        .rsplit('-')
        .next()
        .unwrap_or(&tunnel.tunnel_type);
    let manual_url = if preferred.is_manual() {
        if preferred_info.is_client {
            tunnel.remote_addr.clone()
        } else {
            tunnel.local_addr.clone()
        }
    } else {
        None
    };
    let mut members: Vec<_> = conns
        .iter()
        .filter_map(|entry| {
            let conn = entry.value();
            if conn.is_closed()
                || conn.get_stats().latency_us == 0
                || conn.parallel_target() != target
            {
                return None;
            }
            let info = conn.get_conn_info();
            let info_tunnel = info.tunnel?;
            if info_tunnel.tunnel_type.rsplit('-').next() != Some(protocol) {
                return None;
            }
            if preferred.is_manual()
                && (!conn.is_manual()
                    || info.is_client != preferred_info.is_client
                    || (if info.is_client {
                        info_tunnel.remote_addr
                    } else {
                        info_tunnel.local_addr
                    }) != manual_url)
            {
                return None;
            }
            Some(conn.clone())
        })
        .collect();
    members.sort_by_key(|c| c.cleanup_policy().shared_connection_key());
    if target > 1 {
        let mut slots = std::collections::BTreeSet::new();
        members.retain(|c| slots.insert(c.parallel_member().map_or(0, |m| m.slot)));
    }
    members.truncate(target as usize);
    (target, members)
}

async fn reconcile_parallel_anchor(
    conns: &ConnMap,
    state: &Mutex<ParallelSendState>,
    policy: &super::parallel::ParallelPolicy,
    peer_id: PeerId,
    close: &mpsc::Sender<PeerConnCloseEvent>,
) {
    let bootstraps: Vec<_> = conns
        .iter()
        .filter(|conn| {
            !conn.value().is_closed()
                && conn.value().parallel_member().is_none()
                && super::parallel::remote_offer(&conn.value().get_conn_info().features).is_some()
        })
        .filter_map(|conn| conn.value().cleanup_policy().shared_connection_key())
        .collect();
    let my_peer = conns
        .iter()
        .next()
        .map(|conn| conn.value().get_conn_info().my_peer_id);
    let remote_authority = if my_peer.is_some_and(|my| peer_id < my) {
        policy.remote_anchor()
    } else {
        None
    };
    let candidate = remote_authority.or_else(|| bootstraps.iter().min().copied());
    let Some(candidate) = candidate else { return };
    let anchor = {
        let mut state = state.lock();
        let desired = remote_authority.unwrap_or_else(|| {
            state
                .pool_anchor
                .filter(|old| bootstraps.contains(old))
                .map_or(candidate, |old| old.min(candidate))
        });
        if state.pool_anchor == Some(desired) {
            return;
        };
        state.pool_anchor = Some(desired);
        desired
    };
    let obsolete: Vec<_> = conns
        .iter()
        .filter(|conn| {
            conn.value()
                .parallel_member()
                .is_some_and(|member| (member.pool_id, member.epoch) != anchor)
        })
        .map(|conn| *conn.key())
        .collect();
    for id in obsolete {
        let _ = close.send(PeerConnCloseEvent::Closed(id)).await;
    }
}

#[derive(Debug, Clone)]
enum PeerConnCloseEvent {
    Closed(PeerConnId),
    Redundant {
        conn_id: PeerConnId,
        replacement_id: PeerConnId,
        use_disguise: bool,
        policy: CleanupPolicyPair,
    },
}

/// Revalidate a queued cleanup against the live replacement and the immutable
/// ordering both endpoints advertised. Manual paths use a shared total order;
/// automatic paths must be strictly worse under both protocol preferences.
fn redundant_conn_eligible(
    conn: &PeerConn,
    replacement: &PeerConn,
    flags: &Flags,
    proposed_use_disguise: bool,
    negotiated: Option<bool>,
    proposed_policy: &CleanupPolicyPair,
) -> bool {
    if !flags.close_redundant_conns_when_disguised
        || conn.get_conn_id() == replacement.get_conn_id()
        || replacement.is_closed()
        || replacement.get_stats().latency_us == 0
        || !conn.cleanup_policy().same_ordering_as(proposed_policy)
        || replacement.cleanup_policy() != proposed_policy
        || !proposed_policy.matches_current_flags(flags)
    {
        return false;
    }
    if replacement.is_manual() {
        // RTT and local IDs can select different winners at the two ends.
        // A shared, strict order also prevents crossed cleanup of two manual
        // connections, including when only one endpoint has enabled cleanup.
        return !conn.is_manual()
            || matches!(
                (replacement.cleanup_policy().shared_connection_key(), conn.cleanup_policy().shared_connection_key()),
                (Some(replacement), Some(candidate)) if replacement < candidate
            );
    }
    if !conn.can_retire_as_redundant()
        || negotiated != Some(proposed_use_disguise)
        || use_disguise_preference(flags, negotiated) != proposed_use_disguise
        || proposed_policy.agreed_disguise(flags) != Some(proposed_use_disguise)
    {
        return false;
    }
    let (Some(candidate), Some(replacement)) = (
        conn.get_conn_info().tunnel,
        replacement.get_conn_info().tunnel,
    ) else {
        return false;
    };
    proposed_policy.both_prefer_transport(&replacement.tunnel_type, &candidate.tunnel_type)
}

impl PeerConnCloseEvent {
    fn removable_id(
        &self,
        conns: &ConnMap,
        flags: &Flags,
        negotiated: Option<bool>,
    ) -> Option<PeerConnId> {
        match self {
            Self::Closed(id) => Some(*id),
            Self::Redundant {
                conn_id,
                replacement_id,
                use_disguise,
                policy,
            } => {
                let replacement = conns.get(replacement_id)?.value().clone();
                let conn = conns.get(conn_id)?.value().clone();
                redundant_conn_eligible(
                    &conn,
                    &replacement,
                    flags,
                    *use_disguise,
                    negotiated,
                    policy,
                )
                .then_some(*conn_id)
            }
        }
    }
}

fn conn_latency_sort_key(latency_us: u64, is_hole_punched: bool) -> (bool, u64) {
    (is_hole_punched && latency_us == 0, latency_us)
}

fn preferred_conn_sort_key(
    latency_us: u64,
    is_hole_punched: bool,
    protocol_rank: u8,
) -> (bool, u8, u64) {
    let (unverified, latency) = conn_latency_sort_key(latency_us, is_hole_punched);
    (unverified, protocol_rank, latency)
}

#[cfg(test)]
fn disguised_tunnel_scheme(tunnel_type: &str) -> Option<&'static str> {
    match tunnel_type.rsplit('-').next() {
        Some("wss") => Some("wss"),
        Some("http3") => Some("http3"),
        _ => None,
    }
}

fn use_disguise_preference(flags: &Flags, negotiated: Option<bool>) -> bool {
    !flags.p2p_disguise().forbids()
        && negotiated.unwrap_or(flags.p2p_disguise().prefers() || flags.p2p_disguise().requires())
}

fn conn_protocol_rank(conn: &PeerConn, flags: &Flags) -> Option<u8> {
    if conn.is_manual() {
        return Some(0);
    }
    if conn.is_attached() {
        return None;
    }
    let tunnel = conn.get_conn_info().tunnel?;
    if tunnel.tunnel_type.rsplit('-').next() == Some("ring") {
        return None;
    }
    // The dialing policy controls whether to initiate disguise, not whether
    // an already established WSS/HTTP3 path can replace raw TCP/UDP.
    if conn.parallel_target() > 1 {
        let info = conn.get_conn_info();
        if let Some(rank) = conn.cleanup_policy().parallel_transport_rank(
            &tunnel.tunnel_type,
            info.my_peer_id,
            info.peer_id,
        ) {
            return Some(1 + rank);
        }
    }
    Some(
        1 + p2p_protocol_rank(
            &flags.default_protocol,
            !flags.p2p_disguise().forbids(),
            &tunnel.tunnel_type,
        ),
    )
}

fn select_preferred_conn(conns: &ConnMap, flags: &Flags) -> Option<ArcPeerConn> {
    conns
        .iter()
        .filter(|conn| !conn.value().is_closed())
        .min_by_key(|conn| {
            let conn = conn.value();
            let (unverified, rank, latency) = preferred_conn_sort_key(
                conn.get_stats().latency_us,
                conn.is_hole_punched() || conn.is_manual() || conn.parallel_target() > 1,
                conn_protocol_rank(conn, flags).unwrap_or(1),
            );
            let manual_key = if conn.is_manual() {
                conn.cleanup_policy().shared_connection_key()
            } else {
                None
            };
            let protocol = if conn.parallel_target() > 1 && !conn.is_manual() {
                conn.get_conn_info()
                    .tunnel
                    .map(|t| {
                        t.tunnel_type
                            .rsplit('-')
                            .next()
                            .unwrap_or(&t.tunnel_type)
                            .to_owned()
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (unverified, rank, protocol, manual_key, latency)
        })
        .map(|conn| conn.value().clone())
}

async fn reconcile_peer_connections(
    conns: &ConnMap,
    flags: &Flags,
    negotiated_disguise: &AtomicCell<Option<bool>>,
    default_conn: &ArcSwapOption<PeerConn>,
    update_lock: &Mutex<()>,
    close_event_sender: &mpsc::Sender<PeerConnCloseEvent>,
    peer_id: PeerId,
) {
    let (replacement, use_disguise) = {
        let _update_guard = update_lock.lock();
        let use_disguise = use_disguise_preference(flags, negotiated_disguise.load());
        let selected = select_preferred_conn(conns, flags);
        default_conn.store(selected.clone());
        (selected, use_disguise)
    };
    // A negotiated pool retains its members. Retire the previous protocol only
    // once the selected pool has passed ping on every required member.
    let (target, members) = parallel_members(conns, flags);
    if target > 1 {
        if members.len() < target as usize {
            return;
        }
        if members
            .first()
            .is_some_and(|conn| !conn.parallel_can_trim())
        {
            return;
        }
        let member_ids: Vec<_> = members.iter().map(|c| c.get_conn_id()).collect();
        let stale: Vec<_> = conns
            .iter()
            .filter(|c| {
                !member_ids.contains(c.key())
                    && c.value().get_stats().latency_us > 0
                    && (c.value().can_retire_as_redundant() || c.value().parallel_target() > 1)
            })
            .map(|c| *c.key())
            .collect();
        for id in stale {
            let _ = close_event_sender
                .send(PeerConnCloseEvent::Closed(id))
                .await;
        }
        return;
    }
    if conns
        .iter()
        .any(|conn| conn.value().parallel_member().is_some())
        && members.first().is_some_and(|conn| conn.parallel_can_trim())
    {
        let keep = members[0].get_conn_id();
        let stale: Vec<_> = conns
            .iter()
            .filter(|conn| *conn.key() != keep && !conn.value().is_attached())
            .map(|conn| *conn.key())
            .collect();
        for conn in stale {
            let _ = close_event_sender
                .send(PeerConnCloseEvent::Closed(conn))
                .await;
        }
        return;
    }
    if !flags.close_redundant_conns_when_disguised {
        return;
    }
    let Some(replacement) = replacement else {
        return;
    };
    // A fresh replacement connection is usable for pruning only after a
    // successful round trip: a zero latency means the first ping has not
    // answered yet, and the replacement may still be half-open. Keep the
    // fallback while the first ping is pending - for punched and direct
    // (non-punched) paths alike.
    if replacement.get_stats().latency_us == 0 {
        return;
    }
    let policy = replacement.cleanup_policy().clone();
    if !policy.matches_current_flags(flags)
        || (!replacement.is_manual()
            && (negotiated_disguise.load().is_none()
                || policy.agreed_disguise(flags) != Some(use_disguise)))
    {
        return;
    }
    let negotiated = negotiated_disguise.load();
    let redundant = conns
        .iter()
        .filter(|entry| {
            !entry.value().is_closed()
                && redundant_conn_eligible(
                    entry.value(),
                    &replacement,
                    flags,
                    use_disguise,
                    negotiated,
                    &policy,
                )
        })
        .map(|entry| entry.key().to_owned())
        .collect::<Vec<_>>();

    for conn_id in redundant {
        if replacement.is_closed()
            || !conns.contains_key(&replacement.get_conn_id())
            || (!replacement.is_manual()
                && use_disguise != use_disguise_preference(flags, negotiated_disguise.load()))
        {
            break;
        }
        tracing::info!(
            ?conn_id,
            %peer_id,
            replacement_conn_id = %replacement.get_conn_id(),
            "a preferred connection is ready, closing a redundant connection"
        );
        if let Err(error) = close_event_sender
            .send(PeerConnCloseEvent::Redundant {
                conn_id,
                replacement_id: replacement.get_conn_id(),
                use_disguise,
                policy: policy.clone(),
            })
            .await
        {
            tracing::warn!(?conn_id, %error, "failed to close redundant connection");
        }
    }
}

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
    peer_identity_type: Arc<AtomicCell<Option<PeerIdentityType>>>,
    peer_public_key: Arc<RwLock<Option<Vec<u8>>>>,
    #[allow(dead_code)]
    conn_maintenance_task: AbortOnDropHandle<()>,
    parallel_send: Arc<Mutex<ParallelSendState>>,
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
        let parallel_receiver = super::parallel::ParallelReceiver::new(packet_recv_chan.clone());
        let parallel_policy = Arc::new(super::parallel::ParallelPolicy::default());
        let conn_lifecycle = Arc::new(tokio::sync::Mutex::new(()));
        let receiver = parallel_receiver.clone();
        let receive_conns = conns.clone();
        let parallel_receive_task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
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

        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let context_copy = context.clone();
        let default_conn_copy = default_conn.clone();
        let default_conn_update_lock_copy = default_conn_update_lock.clone();
        let negotiated_disguise_copy = negotiated_disguise.clone();
        let remote_cleanup_requested_copy = remote_cleanup_requested.clone();
        let conn_lifecycle_copy = conn_lifecycle.clone();
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
        let conn_maintenance_task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                crate::foundation::time::sleep(std::time::Duration::from_millis(250)).await;
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
            peer_identity_type,
            peer_public_key,
            conn_maintenance_task,
            parallel_send,
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
                if state.pool_anchor.is_none() {
                    state.epoch = conn.parallel_tx_epoch();
                }
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
        if let Some(conn) = self.default_conn.load_full() {
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

    pub async fn send_msg(&self, msg: ZCPacket) -> Result<(), Error> {
        if super::parallel::stripeable(&msg) {
            let (mut target, mut members) = parallel_members(&self.conns, &self.context.flags());
            if target > 1 && members.is_empty() {
                let ready = tokio::time::timeout(std::time::Duration::from_secs(7), async {
                    loop {
                        if self.conns.iter().all(|conn| conn.value().is_closed()) {
                            return None;
                        }
                        let (target, members) =
                            parallel_members(&self.conns, &self.context.flags());
                        if target == 1 || !members.is_empty() {
                            return Some((target, members));
                        }
                        crate::foundation::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                })
                .await;
                let Ok(Some(ready)) = ready else {
                    return Err(Error::PeerNoConnectionError(self.peer_node_id));
                };
                (target, members) = ready;
            }
            let capable = members.first().is_some_and(|conn| {
                let info = conn.get_conn_info();
                super::parallel::remote_offer(&info.features).is_some()
                    && super::parallel::tx_epoch(&info.features).is_some()
            });
            if (target > 1 || self.parallel_send.lock().sequence > 0)
                && capable
                && !members.is_empty()
            {
                self.parallel_receiver.set_max_rtt_us(
                    members
                        .iter()
                        .map(|c| c.get_stats().latency_us)
                        .max()
                        .unwrap_or(0),
                );
                let (epoch, sequence) = {
                    let mut state = self.parallel_send.lock();
                    state
                        .sent_bytes
                        .retain(|id, _| members.iter().any(|c| c.get_conn_id() == *id));
                    let baseline = state.sent_bytes.values().copied().min().unwrap_or(0);
                    for conn in &members {
                        state
                            .sent_bytes
                            .entry(conn.get_conn_id())
                            .or_insert(baseline);
                    }
                    members.sort_by_key(|c| {
                        (
                            state.sent_bytes.get(&c.get_conn_id()).copied().unwrap_or(0),
                            c.cleanup_policy().shared_connection_key(),
                        )
                    });
                    let sequence = state.sequence;
                    state.sequence = state.sequence.wrapping_add(1);
                    (state.epoch, sequence)
                };
                let info = members[0].get_conn_info();
                let mut packet =
                    super::parallel::wrap(msg, info.my_peer_id, self.peer_node_id, epoch, sequence);
                let bytes = packet.tunnel_payload().len() as u64;
                for conn in &members {
                    match conn.try_send_owned(packet) {
                        Ok(()) => {
                            *self
                                .parallel_send
                                .lock()
                                .sent_bytes
                                .entry(conn.get_conn_id())
                                .or_default() += bytes;
                            return Ok(());
                        }
                        Err(
                            tokio::sync::mpsc::error::TrySendError::Full(p)
                            | tokio::sync::mpsc::error::TrySendError::Closed(p),
                        ) => {
                            packet = p;
                        }
                    }
                }
                // Backpressure waits for any member to become writable; a
                // stalled first member cannot block the other paths.
                use futures::StreamExt;
                let mut ready = futures::stream::FuturesUnordered::new();
                for conn in members {
                    ready.push(async move {
                        let permit = conn.reserve_send().await;
                        (conn, permit)
                    });
                }
                while let Some((conn, permit)) = ready.next().await {
                    if let Ok(permit) = permit {
                        permit.send(packet);
                        *self
                            .parallel_send
                            .lock()
                            .sent_bytes
                            .entry(conn.get_conn_id())
                            .or_default() += bytes;
                        return Ok(());
                    }
                }
                return Err(Error::PeerNoConnectionError(self.peer_node_id));
            }
        }
        let default_conn = self.default_conn.load();
        if let Some(conn) = default_conn.as_ref() {
            conn.send_msg(msg).await?;
            return Ok(());
        }
        drop(default_conn);

        let Some(conn) = self.select_conn() else {
            return Err(Error::PeerNoConnectionError(self.peer_node_id));
        };
        conn.send_msg(msg).await?;

        Ok(())
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
        self.conns.iter().any(|entry| !entry.value().is_closed())
    }

    pub(crate) fn parallel_connection_status(
        &self,
    ) -> crate::proto::common::ParallelConnectionStatus {
        let (target, members) = parallel_members(&self.conns, &self.context.flags());
        let selected = select_preferred_conn(&self.conns, &self.context.flags());
        crate::proto::common::ParallelConnectionStatus {
            target,
            active_conn_ids: members
                .iter()
                .map(|c| c.get_conn_id().to_string())
                .collect(),
            protocol: selected
                .as_ref()
                .and_then(|c| c.get_conn_info().tunnel)
                .map(|t| t.tunnel_type)
                .unwrap_or_default(),
            fallback_reason: if selected.as_ref().is_some_and(|c| {
                super::parallel::remote_offer(&c.get_conn_info().features).is_none()
            }) {
                "peer does not support parallel connections".into()
            } else if self.context.disable_parallel_connections() {
                "parallel connections disabled".into()
            } else if self
                .parallel_policy
                .remote()
                .is_some_and(|remote| remote.disabled)
            {
                "parallel connections disabled by remote peer".into()
            } else if selected
                .as_ref()
                .is_some_and(|conn| conn.parallel_connection_limit() == 1 && !conn.is_manual())
            {
                "connection quota retains the available path during protocol migration".into()
            } else if members.len() < target as usize {
                "waiting for healthy connections".into()
            } else {
                String::new()
            },
        }
    }

    pub(crate) fn reserve_parallel_slot(&self) -> Option<super::parallel::ParallelDialReservation> {
        self.reserve_parallel_slot_for_protocol(None)
    }

    pub(crate) fn reserve_parallel_slot_for_protocol(
        &self,
        requested: Option<&str>,
    ) -> Option<super::parallel::ParallelDialReservation> {
        let (target, members) = parallel_members(&self.conns, &self.context.flags());
        if target <= 1 {
            return None;
        }
        let selected = select_preferred_conn(&self.conns, &self.context.flags())?;
        let info = selected.get_conn_info();
        let tunnel = info.tunnel.as_ref()?;
        let current = tunnel.tunnel_type.rsplit('-').next()?;
        let protocol = requested.unwrap_or(current);
        let upgrading = protocol != current
            && !selected.is_manual()
            && selected.cleanup_policy().parallel_transport_rank(
                protocol,
                info.my_peer_id,
                info.peer_id,
            ) < selected.cleanup_policy().parallel_transport_rank(
                current,
                info.my_peer_id,
                info.peer_id,
            );
        let cap = selected.parallel_connection_limit() as usize;
        let live: Vec<_> = self
            .conns
            .iter()
            .filter(|conn| !conn.value().is_closed())
            .map(|conn| conn.value().clone())
            .collect();
        if live.len() >= cap && (upgrading || protocol == current) {
            let healthy = live
                .iter()
                .filter(|conn| conn.get_stats().latency_us > 0)
                .count();
            let victim = live
                .iter()
                .filter(|conn| {
                    conn.get_stats().latency_us > 0
                        && conn
                            .get_conn_info()
                            .tunnel
                            .is_some_and(|t| t.tunnel_type.rsplit('-').next() != Some(protocol))
                })
                .max_by_key(|conn| conn.parallel_member().map_or(0, |member| member.slot));
            let pending = self.parallel_reservations.len();
            if healthy > 1
                && pending == 0
                && let Some(victim) = victim
            {
                let _ = self
                    .close_event_sender
                    .try_send(PeerConnCloseEvent::Closed(victim.get_conn_id()));
                return None;
            }
            if upgrading || victim.is_some() {
                return None;
            }
        }
        if (!upgrading && members.len() >= target as usize)
            || (requested.is_some() && protocol != current && !upgrading)
        {
            return None;
        }
        let (pool_id, epoch) = self.parallel_send.lock().pool_anchor?;
        let occupied = self
            .conns
            .iter()
            .filter(|c| {
                !c.value().is_closed()
                    && c.value()
                        .get_conn_info()
                        .tunnel
                        .is_some_and(|t| t.tunnel_type.rsplit('-').next() == Some(protocol))
            })
            .map(|c| c.value().parallel_member().map_or(0, |m| m.slot))
            .collect();
        self.parallel_reservations
            .reserve(pool_id, epoch, target, &occupied)
    }

    pub(crate) fn parallel_manual_url(&self) -> Option<url::Url> {
        let selected = select_preferred_conn(&self.conns, &self.context.flags())?;
        let info = selected.get_conn_info();
        if !selected.is_manual() || !info.is_client {
            return None;
        }
        info.tunnel?.remote_addr.map(Into::into)
    }

    pub(crate) fn parallel_default_protocol(&self) -> Option<String> {
        let selected = select_preferred_conn(&self.conns, &self.context.flags())?;
        if selected.parallel_target() <= 1 || selected.is_manual() {
            return None;
        }
        let info = selected.get_conn_info();
        selected
            .cleanup_policy()
            .parallel_default_protocol(info.my_peer_id, info.peer_id)
    }

    /// Records route metadata for revalidating automatic cleanup against the
    /// handshake policy. This does not change established transport rankings.
    pub(crate) fn note_negotiated_disguise(&self, use_disguise: bool) {
        let _update_guard = self.default_conn_update_lock.lock();
        if self.negotiated_disguise.swap(Some(use_disguise)) != Some(use_disguise) {
            self.default_conn.store(None);
        }
    }

    pub(crate) fn has_connection_at_least_as_preferred(&self, target_scheme: &str) -> bool {
        let flags = self.context.flags();
        let (target, members) = parallel_members(&self.conns, &flags);
        if target > 1 {
            if let Some(selected) = select_preferred_conn(&self.conns, &flags) {
                if selected.is_manual() {
                    return selected.get_stats().latency_us > 0;
                }
                let info = selected.get_conn_info();
                let rank = selected.cleanup_policy().parallel_transport_rank(
                    target_scheme,
                    info.my_peer_id,
                    info.peer_id,
                );
                let current_rank = info.tunnel.as_ref().and_then(|t| {
                    selected.cleanup_policy().parallel_transport_rank(
                        &t.tunnel_type,
                        info.my_peer_id,
                        info.peer_id,
                    )
                });
                return members.len() >= target as usize && current_rank <= rank;
            }
            return false;
        }
        let target_rank = 1 + p2p_protocol_rank(
            &flags.default_protocol,
            !flags.p2p_disguise().forbids(),
            target_scheme,
        );
        self.conns.iter().any(|entry| {
            let conn = entry.value();
            !conn.is_closed()
                && (conn.parallel_target() == 1 || conn.get_stats().latency_us > 0)
                && (!conn.is_manual() || conn.get_stats().latency_us > 0)
                && conn_protocol_rank(conn, &flags).is_some_and(|rank| rank <= target_rank)
        })
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
mod parallel_tests;
#[cfg(test)]
mod tests;
