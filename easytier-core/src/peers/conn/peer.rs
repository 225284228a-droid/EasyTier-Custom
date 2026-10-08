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

/// Shared revalidation for closing a redundant automatic connection in
/// favor of `replacement`: the cleanup flag is on, the negotiated disguise
/// preference has not drifted since the proposal, the replacement is alive
/// and verified (its first ping answered), and the candidate is a
/// direct or hole-punch connection strictly worse than the replacement.
fn redundant_conn_eligible(
    conn: &PeerConn,
    replacement: &PeerConn,
    flags: &FlagsInConfig,
    proposed_use_disguise: bool,
    negotiated: Option<bool>,
    proposed_policy: &CleanupPolicyPair,
) -> bool {
    if !flags.close_redundant_conns_when_disguised
        || negotiated != Some(proposed_use_disguise)
        || use_disguise_preference(flags, negotiated) != proposed_use_disguise
    {
        return false;
    }
    if replacement.is_closed()
        || replacement.get_stats().latency_us == 0
        || !conn.can_retire_as_redundant()
        || conn.cleanup_policy() != proposed_policy
        || replacement.cleanup_policy() != proposed_policy
        || proposed_policy.agreed_disguise(flags) != Some(proposed_use_disguise)
    {
        return false;
    }
    let Some(replacement_rank) =
        conn_protocol_rank(replacement, &flags.default_protocol, proposed_use_disguise)
    else {
        return false;
    };
    conn_protocol_rank(conn, &flags.default_protocol, proposed_use_disguise)
        .is_some_and(|rank| rank > replacement_rank)
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

fn conn_protocol_rank(conn: &PeerConn, default_protocol: &str, use_disguise: bool) -> Option<u8> {
    if conn.is_attached() {
        return None;
    }
    let tunnel = conn.get_conn_info().tunnel?;
    if tunnel.tunnel_type.rsplit('-').next() == Some("ring") {
        return None;
    }
    Some(p2p_protocol_rank(
        default_protocol,
        use_disguise,
        &tunnel.tunnel_type,
    ))
}

fn select_preferred_conn(
    conns: &ConnMap,
    flags: &FlagsInConfig,
    use_disguise: bool,
) -> Option<ArcPeerConn> {
    conns
        .iter()
        .filter(|conn| !conn.value().is_closed())
        .min_by_key(|conn| {
            let conn = conn.value();
            preferred_conn_sort_key(
                conn.get_stats().latency_us,
                conn.is_hole_punched(),
                conn_protocol_rank(conn, &flags.default_protocol, use_disguise).unwrap_or(0),
            )
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
        let selected = select_preferred_conn(conns, flags, use_disguise);
        default_conn.store(selected.clone());
        (selected, use_disguise)
    };
    if !flags.close_redundant_conns_when_disguised || negotiated_disguise.load().is_none() {
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
    if policy.agreed_disguise(flags) != Some(use_disguise) {
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
            || use_disguise != use_disguise_preference(flags, negotiated_disguise.load())
        {
            break;
        }
        tracing::info!(
            ?conn_id,
            %peer_id,
            replacement_conn_id = %replacement.get_conn_id(),
            "a preferred connection is ready, closing a redundant automatic connection"
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

        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let context_copy = context.clone();
        let default_conn_copy = default_conn.clone();
        let default_conn_update_lock_copy = default_conn_update_lock.clone();
        let negotiated_disguise_copy = negotiated_disguise.clone();
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
        let selected = select_preferred_conn(
            &self.conns,
            &flags,
            use_disguise_preference(&flags, self.negotiated_disguise.load()),
        );

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

    /// Records the disguise preference negotiated with this peer, derived
    /// from route metadata that knows both endpoints' policy. A value change
    /// invalidates the cached default connection so the next send reselects
    /// under the new preference.
    pub(crate) fn note_negotiated_disguise(&self, use_disguise: bool) {
        let _update_guard = self.default_conn_update_lock.lock();
        if self.negotiated_disguise.swap(Some(use_disguise)) != Some(use_disguise) {
            self.default_conn.store(None);
        }
    }

    pub(crate) fn has_connection_at_least_as_preferred(
        &self,
        target_scheme: &str,
        use_disguise: Option<bool>,
    ) -> bool {
        let default_protocol = self.context.flags().default_protocol;
        // `None` means the route metadata is missing. Rank with the last
        // negotiated preference and do NOT overwrite it: a metadata gap
        // flipping the preference to false would make the next reconcile
        // pass tear down established disguised connections. Callers that
        // observed fresh route metadata update the preference explicitly
        // via `note_negotiated_disguise` (see `PeerMap`).
        let use_disguise = use_disguise.unwrap_or_else(|| {
            self.negotiated_disguise
                .load()
                .unwrap_or(use_disguise_preference(&self.context.flags(), None))
        });
        let target_rank = p2p_protocol_rank(&default_protocol, use_disguise, target_scheme);
        self.conns.iter().any(|entry| {
            let conn = entry.value();
            !conn.is_closed()
                && conn_protocol_rank(conn, &default_protocol, use_disguise)
                    .is_some_and(|rank| rank <= target_rank)
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
mod tests {
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
            origin,
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
            ("udp", false) => ("udp", "wss", PeerConnectionOrigin::UdpHolePunch),
            ("tcp", false) => ("tcp", "http3", PeerConnectionOrigin::TcpHolePunch),
            _ => unreachable!(),
        };
        let context: ArcPeerContext =
            Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
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

    #[tokio::test]
    async fn cleanup_keeps_noncooperating_peers_and_mismatched_connection_snapshots() {
        let flags = FlagsInConfig {
            default_protocol: "tcp".to_owned(),
            close_redundant_conns_when_disguised: true,
            ..Default::default()
        };
        let context: ArcPeerContext =
            Arc::new(NoopPeerContext::default().with_flags(flags.clone()));
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
                    handshaken_connection(context, preferred, PeerConnectionOrigin::Direct, true)
                        .await;
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
                let (conn, mut remote) = handshaken_connection(
                    context.clone(),
                    scheme,
                    PeerConnectionOrigin::Direct,
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
            peer.close_event_sender
                .try_send(PeerConnCloseEvent::Redundant {
                    conn_id: ids[0],
                    replacement_id: ids[1],
                    use_disguise: false,
                    policy: peer.conns.get(&ids[0]).unwrap().cleanup_policy().clone(),
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
                let (conn, mut remote) = handshaken_connection(
                    context.clone(),
                    scheme,
                    PeerConnectionOrigin::Direct,
                    true,
                )
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
        let context: ArcPeerContext =
            Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
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
                policy: peer.conns.get(&ids[1]).unwrap().cleanup_policy().clone(),
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
                    .get(&fallback_id)
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
        let context: ArcPeerContext =
            Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
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
                handshaken_connection(context.clone(), "udp", PeerConnectionOrigin::Direct, true)
                    .await;
            let udp_id = udp.get_conn_id();
            let (wss, mut wss_remote) =
                handshaken_connection(context, "wss", PeerConnectionOrigin::TcpHolePunch, false)
                    .await;
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
                    policy: peer.conns.get(&wss_id).unwrap().cleanup_policy().clone(),
                })
                .unwrap();
            if preference_changed {
                peer.note_negotiated_disguise(true);
                peer.close_event_sender
                    .try_send(PeerConnCloseEvent::Redundant {
                        conn_id: udp_id,
                        replacement_id: wss_id,
                        use_disguise: true,
                        policy: peer.conns.get(&udp_id).unwrap().cleanup_policy().clone(),
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
    async fn cleanup_preserves_manual_inbound_attached_and_equal_rank_connections() {
        let context: ArcPeerContext =
            Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
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
            ("wss", PeerConnectionOrigin::Manual, true),
            ("wss", PeerConnectionOrigin::Listener, false),
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
    async fn negotiated_peer_policy_overrides_local_disguise_preference() {
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
            // Retiring the wss conn requires its udp replacement to be
            // verified by a first ping round trip.
            wait_until_latency_verified(&peer, &[udp_id]).await;
            assert!(peer.conns.contains_key(&wss_id));
            assert!(peer.conns.contains_key(&udp_id));
            // Route metadata negotiated use_disguise=false despite the local
            // prefer flag; the negotiated value wins.
            peer.note_negotiated_disguise(false);
            assert!(peer.has_connection_at_least_as_preferred("udp", Some(false)));
            assert_eq!(peer.select_conn().unwrap().get_conn_id(), udp_id);
            peer.reconcile_connections().await;
            assert_eq!(peer.get_default_conn_id(), udp_id);
            if cleanup_enabled {
                tokio::time::timeout(Duration::from_secs(1), async {
                    while peer.conns.contains_key(&wss_id) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
            }
            assert_eq!(peer.conns.contains_key(&wss_id), !cleanup_enabled);
            assert!(peer.conns.contains_key(&udp_id));
        }
    }

    #[tokio::test]
    async fn missing_route_metadata_keeps_negotiated_disguise_stable() {
        let context: ArcPeerContext =
            Arc::new(NoopPeerContext::default().with_flags(FlagsInConfig {
                default_protocol: "udp".to_owned(),
                prefer_wss_http3_for_p2p: true,
                close_redundant_conns_when_disguised: true,
                ..Default::default()
            }));
        let (local_tx, _local_rx) = create_packet_recv_chan();
        let peer = Peer::new(2, local_tx, context.clone());
        let (http3, _remote) =
            handshaken_connection(context.clone(), "http3", PeerConnectionOrigin::Direct, true)
                .await;
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
}
