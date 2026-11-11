//! Scheduling for a peer's connections: plans which member sends a packet,
//! selects the preferred connection, and reconciles redundant ones.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use arc_swap::ArcSwapOption;
use crossbeam_utils::atomic::AtomicCell;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::super::{
    cleanup_policy::CleanupPolicyPair,
    parallel,
    peer_conn::{PeerConn, PeerConnId},
};
use super::{ArcPeerConn, ConnMap, Peer};
use crate::{
    config::{PeerId, p2p_protocol_rank},
    packet::ZCPacket,
    peers::error::Error,
    proto::common::Flags,
};

pub(super) struct ParallelSendState {
    pub(super) epoch: uuid::Uuid,
    pub(super) sequence: u64,
    /// When each member is expected to have transmitted what was queued on it.
    pub(super) busy_until: HashMap<PeerConnId, Instant>,
    pub(super) sent_bytes: HashMap<PeerConnId, u64>,
    pub(super) pool_anchor: Option<(uuid::Uuid, uuid::Uuid)>,
}
impl Default for ParallelSendState {
    fn default() -> Self {
        Self {
            epoch: uuid::Uuid::new_v4(),
            sequence: 0,
            busy_until: HashMap::new(),
            sent_bytes: HashMap::new(),
            pool_anchor: None,
        }
    }
}

/// How fast a member transmits and how far away the peer is over it.
#[derive(Clone, Copy)]
pub(super) struct LaneModel {
    pub(super) bps: u64,
    pub(super) one_way: Duration,
}

impl LaneModel {
    pub(super) fn transmit_time(self, bytes: usize) -> Duration {
        Duration::from_micros((bytes as u64).saturating_mul(8_000_000) / self.bps.max(1))
    }

    /// When a packet queued now would arrive at the peer. The member's queue
    /// is modelled from its estimated rate: backlog in the kernel or the
    /// network is not observable, and datagram paths never queue locally.
    pub(super) fn arrival(
        self,
        busy_until: Option<Instant>,
        now: Instant,
        bytes: usize,
    ) -> Instant {
        busy_until.map_or(now, |busy| busy.max(now)) + self.transmit_time(bytes) + self.one_way
    }
}

/// One member with its delivery model, refreshed with the send plan.
pub(super) struct Lane {
    pub(super) conn: ArcPeerConn,
    /// Absent while any member's rate is unknown.
    pub(super) model: Option<LaneModel>,
    pub(super) order_key: Option<(uuid::Uuid, uuid::Uuid)>,
}

/// What `send_msg` needs for every packet. Computing it walks every
/// connection and copies connection info, so it is cached and recomputed only
/// after the generation changes: on connection admission and removal, and on
/// the maintenance tick, which picks up latency verification and renegotiated
/// parallel targets.
pub(super) struct SendPlan {
    pub(super) generation: u64,
    pub(super) target: u32,
    pub(super) members: Vec<Lane>,
    /// Stripe packets across `members` instead of using the default connection.
    pub(super) stripe: bool,
    pub(super) my_peer_id: PeerId,
}

/// Invalidation also drops the cached plan: it holds its members, and a removed
/// connection must be released so its tunnel closes. The generation rejects a
/// plan that a concurrent sender computed before the change.
#[derive(Default)]
pub(super) struct SendPlanCache {
    pub(super) plan: ArcSwapOption<SendPlan>,
    pub(super) generation: AtomicU64,
}

impl SendPlanCache {
    pub(super) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.plan.store(None);
    }
}

pub(super) fn compute_send_plan(
    conns: &ConnMap,
    flags: &Flags,
    generation: u64,
    parallel_send: &Mutex<ParallelSendState>,
    receiver: &parallel::ParallelReceiver,
) -> SendPlan {
    let (target, members) = parallel_members(conns, flags);
    let first_info = members.first().map(|conn| conn.get_conn_info());
    let capable = first_info.as_ref().is_some_and(|info| {
        parallel::remote_offer(&info.features).is_some()
            && parallel::tx_epoch(&info.features).is_some()
    });
    let mut state = parallel_send.lock();
    // Once striping has started it continues on a single member, so the
    // receiver keeps one ordered sequence.
    let stripe = capable && !members.is_empty() && (target > 1 || state.sequence > 0);
    if stripe {
        let member = |id: &PeerConnId| members.iter().any(|c| c.get_conn_id() == *id);
        state.busy_until.retain(|id, _| member(id));
        state.sent_bytes.retain(|id, _| member(id));
        let baseline = state.sent_bytes.values().copied().min().unwrap_or(0);
        for conn in &members {
            state
                .sent_bytes
                .entry(conn.get_conn_id())
                .or_insert(baseline);
        }
    }
    drop(state);
    let stats: Vec<_> = members.iter().map(|conn| conn.get_stats()).collect();
    if stripe {
        receiver.set_max_rtt_us(
            stats
                .iter()
                .map(|stats| stats.latency_us)
                .max()
                .unwrap_or(0),
        );
    }
    // Only rates measured from delivery are trusted for scheduling; a window
    // estimate may promise more than a shaped path delivers.
    let delivered: Vec<_> = members.iter().map(|conn| conn.delivered_tx_bps()).collect();
    let rates_known = delivered.iter().all(|bps| *bps > 0);
    let members = members
        .into_iter()
        .zip(stats.into_iter().zip(delivered))
        .map(|(conn, (stats, bps))| Lane {
            order_key: conn.cleanup_policy().shared_connection_key(),
            model: rates_known.then(|| LaneModel {
                bps,
                one_way: Duration::from_micros(stats.latency_us / 2),
            }),
            conn,
        })
        .collect();
    SendPlan {
        generation,
        target,
        members,
        stripe,
        my_peer_id: first_info.map_or(0, |info| info.my_peer_id),
    }
}

pub(super) fn parallel_members(conns: &ConnMap, flags: &Flags) -> (u32, Vec<ArcPeerConn>) {
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

pub(super) async fn reconcile_parallel_anchor(
    conns: &ConnMap,
    state: &Mutex<ParallelSendState>,
    policy: &parallel::ParallelPolicy,
    peer_id: PeerId,
    close: &mpsc::Sender<PeerConnCloseEvent>,
) {
    let bootstraps: Vec<_> = conns
        .iter()
        .filter(|conn| {
            !conn.value().is_closed()
                && conn.value().parallel_member().is_none()
                && parallel::remote_offer(&conn.value().get_conn_info().features).is_some()
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
pub(super) enum PeerConnCloseEvent {
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
    pub(super) fn removable_id(
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

pub(super) fn conn_latency_sort_key(latency_us: u64, is_hole_punched: bool) -> (bool, u64) {
    (is_hole_punched && latency_us == 0, latency_us)
}

pub(super) fn preferred_conn_sort_key(
    latency_us: u64,
    is_hole_punched: bool,
    protocol_rank: u8,
) -> (bool, u8, u64) {
    let (unverified, latency) = conn_latency_sort_key(latency_us, is_hole_punched);
    (unverified, protocol_rank, latency)
}

fn use_disguise_preference(flags: &Flags, negotiated: Option<bool>) -> bool {
    let mode = flags.p2p_disguise();
    !mode.forbids() && negotiated.unwrap_or(mode.initiates())
}

pub(super) fn conn_protocol_rank(conn: &PeerConn, flags: &Flags) -> Option<u8> {
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

pub(super) fn select_preferred_conn(conns: &ConnMap, flags: &Flags) -> Option<ArcPeerConn> {
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

pub(super) async fn reconcile_peer_connections(
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

impl Peer {
    pub(super) fn send_plan(&self) -> Arc<SendPlan> {
        let generation = self.send_plan.generation.load(Ordering::Acquire);
        if let Some(plan) = self.send_plan.plan.load().as_ref()
            && plan.generation == generation
        {
            return plan.clone();
        }
        let plan = Arc::new(compute_send_plan(
            &self.conns,
            &self.context.flags(),
            generation,
            &self.parallel_send,
            &self.parallel_receiver,
        ));
        self.send_plan.plan.store(Some(plan.clone()));
        // An invalidation raced with this computation: do not keep its
        // members alive in the cache.
        if self.send_plan.generation.load(Ordering::Acquire) != generation {
            self.send_plan.plan.store(None);
        }
        plan
    }

    /// Sends on the member expected to deliver the packet first, from its
    /// modelled queue, rate and one-way delay. Until every member's rate is
    /// known, members take turns by bytes sent, which also gives each one the
    /// traffic its rate is measured from. A member that cannot take the
    /// packet now yields to the next.
    pub(super) async fn send_striped(&self, msg: ZCPacket, plan: &SendPlan) -> Result<(), Error> {
        let bytes = msg.buf_len() + crate::packet::PARALLEL_PACKET_OVERHEAD;
        let now = Instant::now();
        let record_sent = |state: &mut ParallelSendState, lane: &Lane| {
            let id = lane.conn.get_conn_id();
            *state.sent_bytes.entry(id).or_default() += bytes as u64;
            if let Some(model) = lane.model {
                let busy = state.busy_until.entry(id).or_insert(now);
                *busy = (*busy).max(Instant::now()) + model.transmit_time(bytes);
            }
        };
        // Assigning the sequence and queueing it under one lock keeps each
        // member's queue in sequence order, which lets the receiver treat a
        // gap behind an ordered member as loss.
        let (order, packet) = {
            let mut state = self.parallel_send.lock();
            let mut order: Vec<_> = plan
                .members
                .iter()
                .map(|lane| {
                    let id = lane.conn.get_conn_id();
                    let arrival = lane.model.map_or(now, |model| {
                        model.arrival(state.busy_until.get(&id).copied(), now, bytes)
                    });
                    let sent = state.sent_bytes.get(&id).copied().unwrap_or(0);
                    ((arrival, sent, lane.order_key), lane)
                })
                .collect();
            order.sort_by_key(|(key, _)| *key);
            let sequence = state.sequence;
            state.sequence = state.sequence.wrapping_add(1);
            let mut packet = parallel::wrap(
                msg,
                plan.my_peer_id,
                self.peer_node_id,
                state.epoch,
                sequence,
            );
            for &(_, lane) in &order {
                match lane.conn.try_send_owned(packet) {
                    Ok(()) => {
                        record_sent(&mut state, lane);
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
            (order, packet)
        };
        // Backpressure waits for any member to become writable; a stalled
        // first member cannot block the other paths.
        use futures::StreamExt;
        let mut ready = futures::stream::FuturesUnordered::new();
        for (_, lane) in order {
            ready.push(async move {
                let permit = lane.conn.reserve_send().await;
                (lane, permit)
            });
        }
        while let Some((lane, permit)) = ready.next().await {
            if let Ok(permit) = permit {
                permit.send(packet);
                record_sent(&mut self.parallel_send.lock(), lane);
                return Ok(());
            }
        }
        // Every member is closed: recompute for the next packet.
        self.send_plan.invalidate();
        Err(Error::PeerNoConnectionError(self.peer_node_id))
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
            fallback_reason: if selected
                .as_ref()
                .is_some_and(|c| parallel::remote_offer(&c.get_conn_info().features).is_none())
            {
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

    pub(crate) fn reserve_parallel_slot(&self) -> Option<parallel::ParallelDialReservation> {
        self.reserve_parallel_slot_for_protocol(None)
    }

    pub(crate) fn reserve_parallel_slot_for_protocol(
        &self,
        requested: Option<&str>,
    ) -> Option<parallel::ParallelDialReservation> {
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
}
