use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossbeam::atomic::AtomicCell;
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
    packet::ZCPacket,
    peers::error::Error,
    proto::{common::FlagsInConfig, core_peer::peer::PeerConnInfo, peer_rpc::PeerIdentityType},
};
use tokio_util::task::AbortOnDropHandle;

type ArcPeerConn = Arc<PeerConn>;
type ConnMap = Arc<DashMap<PeerConnId, ArcPeerConn>>;

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
    flags: &FlagsInConfig,
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
        flags: &FlagsInConfig,
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

fn use_disguise_preference(flags: &FlagsInConfig, negotiated: Option<bool>) -> bool {
    !flags.disable_wss_http3_for_p2p
        && negotiated
            .unwrap_or(flags.prefer_wss_http3_for_p2p || flags.only_use_wss_http3_for_hole_punching)
}

fn conn_protocol_rank(conn: &PeerConn, flags: &FlagsInConfig) -> Option<u8> {
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
    Some(
        1 + p2p_protocol_rank(
            &flags.default_protocol,
            !flags.disable_wss_http3_for_p2p,
            &tunnel.tunnel_type,
        ),
    )
}

fn select_preferred_conn(conns: &ConnMap, flags: &FlagsInConfig) -> Option<ArcPeerConn> {
    conns
        .iter()
        .filter(|conn| !conn.value().is_closed())
        .min_by_key(|conn| {
            let conn = conn.value();
            let (unverified, rank, latency) = preferred_conn_sort_key(
                conn.get_stats().latency_us,
                conn.is_hole_punched() || conn.is_manual(),
                conn_protocol_rank(conn, flags).unwrap_or(1),
            );
            let manual_key = if conn.is_manual() {
                conn.cleanup_policy().shared_connection_key()
            } else {
                None
            };
            (unverified, rank, manual_key, latency)
        })
        .map(|conn| conn.value().clone())
}

async fn reconcile_peer_connections(
    conns: &ConnMap,
    flags: &FlagsInConfig,
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

    shutdown_notifier: Arc<tokio::sync::Notify>,

    default_conn: Arc<ArcSwapOption<PeerConn>>,
    default_conn_update_lock: Arc<Mutex<()>>,
    negotiated_disguise: Arc<AtomicCell<Option<bool>>>,
    remote_cleanup_requested: Arc<AtomicCell<bool>>,
    peer_identity_type: Arc<AtomicCell<Option<PeerIdentityType>>>,
    peer_public_key: Arc<RwLock<Option<Vec<u8>>>>,
    #[allow(dead_code)]
    conn_maintenance_task: AbortOnDropHandle<()>,
}

impl Peer {
    pub(crate) fn new(
        peer_node_id: PeerId,
        packet_recv_chan: PacketRecvChan,
        context: ArcPeerContext,
    ) -> Self {
        let conns: ConnMap = Arc::new(DashMap::new());
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

        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let context_copy = context.clone();
        let default_conn_copy = default_conn.clone();
        let default_conn_update_lock_copy = default_conn_update_lock.clone();
        let negotiated_disguise_copy = negotiated_disguise.clone();
        let remote_cleanup_requested_copy = remote_cleanup_requested.clone();
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
                                }
                                removed_conn
                            };

                            if let Some((_, conn)) = removed_conn {
                                context_copy.issue_event(PeerEvent::PeerConnRemoved(
                                    conn.get_conn_info(),
                                ));
                                shrink_dashmap(&conns_copy, Some(4));
                                if conns_copy.is_empty() {
                                    remote_cleanup_requested_copy.store(false);
                                    peer_identity_type_copy.store(None);
                                    *peer_public_key_copy.write() = None;
                                }
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
        let conn_maintenance_task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                crate::foundation::time::sleep(std::time::Duration::from_secs(5)).await;
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

            shutdown_notifier,
            default_conn,
            default_conn_update_lock,
            negotiated_disguise,
            remote_cleanup_requested,
            peer_identity_type,
            peer_public_key,
            conn_maintenance_task,
        }
    }

    pub async fn add_peer_conn(&self, mut conn: PeerConn) -> Result<(), Error> {
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

        // A peer may enable cleanup after the surviving manual connection's
        // handshake. Use the latest supported offer (including opt-out), so
        // retired URLs neither churn nor remain suppressed by an old offer.
        if let Some(enabled) = conn.cleanup_policy().remote_cleanup_enabled() {
            self.remote_cleanup_requested.store(enabled);
        }
        conn.start_recv_loop(self.packet_recv_chan.clone()).await;
        conn.start_pingpong();
        self.conns.insert(conn.get_conn_id(), Arc::new(conn));

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

    /// Records route metadata for revalidating automatic cleanup against the
    /// handshake policy. This does not change established transport rankings.
    pub(crate) fn note_negotiated_disguise(&self, use_disguise: bool) {
        let _update_guard = self.default_conn_update_lock.lock();
        if self.negotiated_disguise.swap(Some(use_disguise)) != Some(use_disguise) {
            self.default_conn.store(None);
        }
    }

    pub(crate) fn has_connection_at_least_as_preferred(
        &self,
        target_scheme: &str,
        _use_disguise: Option<bool>,
    ) -> bool {
        let flags = self.context.flags();
        let target_rank = 1 + p2p_protocol_rank(
            &flags.default_protocol,
            !flags.disable_wss_http3_for_p2p,
            target_scheme,
        );
        self.conns.iter().any(|entry| {
            let conn = entry.value();
            !conn.is_closed()
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
mod tests;
