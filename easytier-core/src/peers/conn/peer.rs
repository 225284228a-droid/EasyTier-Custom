use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossbeam::atomic::AtomicCell;
use dashmap::{DashMap, DashSet};
use parking_lot::{Mutex, RwLock};

use tokio::{select, sync::mpsc};

use tracing::Instrument;

use super::peer_conn::{PeerConn, PeerConnId};
use crate::peers::{
    PacketRecvChan, PeerConnSource,
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

#[derive(Debug, Clone, Copy)]
enum PeerConnCloseEvent {
    Closed(PeerConnId),
    Redundant {
        conn_id: PeerConnId,
        replacement_id: PeerConnId,
        use_disguise: bool,
    },
}

impl PeerConnCloseEvent {
    fn removable_id(
        &self,
        conns: &ConnMap,
        flags: &FlagsInConfig,
        negotiated: Option<bool>,
    ) -> Option<PeerConnId> {
        let Self::Redundant {
            conn_id,
            replacement_id,
            use_disguise,
        } = *self
        else {
            let Self::Closed(id) = *self else {
                unreachable!()
            };
            return Some(id);
        };
        if !flags.close_redundant_conns_when_disguised
            || negotiated != Some(use_disguise)
            || use_disguise_preference(flags, negotiated) != use_disguise
        {
            return None;
        }
        let replacement = conns.get(&replacement_id)?.value().clone();
        let conn = conns.get(&conn_id)?.value().clone();
        if replacement.is_closed()
            || conn_latency_sort_key(
                replacement.get_stats().latency_us,
                replacement.is_hole_punched(),
            )
            .0
            || conn.conn_source() != PeerConnSource::Automatic
        {
            return None;
        }
        let replacement_rank =
            conn_protocol_rank(&replacement, &flags.default_protocol, use_disguise)?;
        let rank = conn_protocol_rank(&conn, &flags.default_protocol, use_disguise)?;
        (rank > replacement_rank).then_some(conn_id)
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

fn disguised_tunnel_scheme(tunnel_type: &str) -> Option<&'static str> {
    match tunnel_type.rsplit('-').next() {
        Some("wss") => Some("wss"),
        Some("http3") => Some("http3"),
        _ => None,
    }
}

/// The disguised transport of a connection, if it uses one. Tunnel types may
/// carry a resolution prefix (`http-txt-wss`), so only the last segment counts.
fn conn_disguised_scheme(conn: &PeerConn) -> Option<&'static str> {
    let tunnel_type = conn.get_conn_info().tunnel.as_ref()?.tunnel_type.clone();
    disguised_tunnel_scheme(&tunnel_type)
}

fn conn_is_disguised(conn: &PeerConn) -> bool {
    conn_disguised_scheme(conn).is_some()
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
    // A freshly punched path is usable for pruning only after a successful
    // round trip. Keep the fallback while its first ping is still pending.
    if conn_latency_sort_key(
        replacement.get_stats().latency_us,
        replacement.is_hole_punched(),
    )
    .0
    {
        return;
    }
    let Some(replacement_rank) =
        conn_protocol_rank(&replacement, &flags.default_protocol, use_disguise)
    else {
        return;
    };
    let redundant = conns
        .iter()
        .filter(|entry| {
            let conn = entry.value();
            !conn.is_closed()
                && conn.conn_source() == PeerConnSource::Automatic
                && conn_protocol_rank(conn, &flags.default_protocol, use_disguise)
                    .is_some_and(|rank| rank > replacement_rank)
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

    pub fn has_disguised_conn(&self) -> bool {
        self.conns
            .iter()
            .any(|entry| !entry.value().is_closed() && conn_is_disguised(entry.value()))
    }

    pub(crate) fn has_connection_at_least_as_preferred(
        &self,
        target_scheme: &str,
        use_disguise: bool,
    ) -> bool {
        // Routing knows both endpoints' policy; retain its negotiation so
        // forwarding and cleanup use the same preference as the schedulers.
        {
            let _update_guard = self.default_conn_update_lock.lock();
            if self.negotiated_disguise.swap(Some(use_disguise)) != Some(use_disguise) {
                self.default_conn.store(None);
            }
        }
        let default_protocol = self.context.flags().default_protocol;
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
            PeerConnSource, PeerConnectionOrigin,
            conn::{peer_conn::PeerConn, peer_session::PeerSessionStore},
            context::ArcPeerContext,
            create_packet_recv_chan,
            test_support::NoopPeerContext,
        },
        proto::common::{FlagsInConfig, TunnelInfo},
        tunnel::ring::{RingTunnel, create_ring_socket_pair},
    };

    use super::{Peer, PeerConnCloseEvent, conn_latency_sort_key, disguised_tunnel_scheme};

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
        source: PeerConnSource,
        origin: PeerConnectionOrigin,
        is_hole_punched: bool,
    ) -> (PeerConn, PeerConn) {
        let (client_socket, server_socket) = create_ring_socket_pair(64);
        let tunnel_info = Some(TunnelInfo {
            tunnel_type: scheme.to_owned(),
            ..Default::default()
        });
        let session_store = Arc::new(PeerSessionStore::new());
        let mut client = PeerConn::new_with_peer_id_hint_and_origin(
            1,
            context,
            Box::new(RingTunnel::new(client_socket, tunnel_info.clone())),
            None,
            session_store.clone(),
            origin,
            source,
        );
        client.set_is_hole_punched(is_hole_punched);
        let mut server = PeerConn::new(
            2,
            Arc::new(NoopPeerContext::default()),
            Box::new(RingTunnel::new(server_socket, tunnel_info)),
            session_store,
        );
        let (client_result, server_result) = tokio::join!(
            client.do_handshake_as_client(),
            server.do_handshake_as_server_ext(|_, _| Ok(())),
        );
        client_result.unwrap();
        server_result.unwrap();
        (client, server)
    }

    fn data_packet() -> ZCPacket {
        let mut packet = ZCPacket::new_with_payload(b"preferred-path");
        packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        packet
    }

    async fn check_preferred_transition(default_protocol: &str, use_disguise: bool, strict: bool) {
        let (preferred, fallback) = match (default_protocol, use_disguise) {
            ("udp", true) => ("http3", "wss"),
            ("tcp", true) => ("wss", "http3"),
            ("udp", false) => ("udp", "wss"),
            ("tcp", false) => ("tcp", "http3"),
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
            PeerConnSource::Automatic,
            PeerConnectionOrigin::Network,
            false,
        )
        .await;
        let old_id = old.get_conn_id();
        let (old_tx, mut old_rx) = create_packet_recv_chan();
        old_remote.start_recv_loop(old_tx).await;
        peer.add_peer_conn(old).await.unwrap();
        assert!(!peer.has_connection_at_least_as_preferred(preferred, use_disguise));

        let (replacement, mut replacement_remote) = handshaken_connection(
            context.clone(),
            preferred,
            PeerConnSource::Automatic,
            PeerConnectionOrigin::Network,
            true,
        )
        .await;
        let replacement_id = replacement.get_conn_id();
        peer.add_peer_conn(replacement).await.unwrap();
        assert!(peer.has_connection_at_least_as_preferred(preferred, use_disguise));
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
        assert!(!peer.has_connection_at_least_as_preferred(preferred, use_disguise));
        let (recovered, mut recovered_remote) = handshaken_connection(
            context,
            fallback,
            PeerConnSource::Automatic,
            PeerConnectionOrigin::Network,
            false,
        )
        .await;
        let (recovered_tx, mut recovered_rx) = create_packet_recv_chan();
        recovered_remote.start_recv_loop(recovered_tx).await;
        peer.add_peer_conn(recovered).await.unwrap();
        assert!(!peer.has_connection_at_least_as_preferred(preferred, use_disguise));
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
            PeerConnSource::Automatic,
            PeerConnectionOrigin::Network,
            false,
        )
        .await;
        let fallback_id = fallback.get_conn_id();
        let (remote_tx, mut remote_rx) = create_packet_recv_chan();
        fallback_remote.start_recv_loop(remote_tx).await;
        peer.add_peer_conn(fallback).await.unwrap();
        assert!(!peer.has_connection_at_least_as_preferred("http3", true));
        let (replacement, replacement_remote) = handshaken_connection(
            context,
            "http3",
            PeerConnSource::Automatic,
            PeerConnectionOrigin::Network,
            true,
        )
        .await;
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
        assert!(!peer.has_connection_at_least_as_preferred("http3", true));
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
            let (udp, _udp_remote) = handshaken_connection(
                context.clone(),
                "udp",
                PeerConnSource::Automatic,
                PeerConnectionOrigin::Network,
                false,
            )
            .await;
            let udp_id = udp.get_conn_id();
            let (wss, _wss_remote) = handshaken_connection(
                context,
                "wss",
                PeerConnSource::Automatic,
                PeerConnectionOrigin::Network,
                false,
            )
            .await;
            let wss_id = wss.get_conn_id();
            peer.add_peer_conn(udp).await.unwrap();
            peer.add_peer_conn(wss).await.unwrap();
            peer.has_connection_at_least_as_preferred("udp", false);
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
                })
                .unwrap();
            if preference_changed {
                peer.has_connection_at_least_as_preferred("http3", true);
                peer.close_event_sender
                    .try_send(PeerConnCloseEvent::Redundant {
                        conn_id: udp_id,
                        replacement_id: wss_id,
                        use_disguise: true,
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
        peer.has_connection_at_least_as_preferred("udp", false);
        let mut remote_conns = Vec::new();
        let mut kept_ids = Vec::new();
        for (scheme, source, origin) in [
            ("wss", PeerConnSource::Manual, PeerConnectionOrigin::Network),
            (
                "wss",
                PeerConnSource::Inbound,
                PeerConnectionOrigin::Network,
            ),
            (
                "udp",
                PeerConnSource::Automatic,
                PeerConnectionOrigin::Network,
            ),
            (
                "udp",
                PeerConnSource::Automatic,
                PeerConnectionOrigin::Network,
            ),
            (
                "ring",
                PeerConnSource::Automatic,
                PeerConnectionOrigin::Attached,
            ),
        ] {
            let (conn, remote) =
                handshaken_connection(context.clone(), scheme, source, origin, false).await;
            kept_ids.push(conn.get_conn_id());
            remote_conns.push(remote);
            peer.add_peer_conn(conn).await.unwrap();
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
            let (wss, _wss_remote) = handshaken_connection(
                context.clone(),
                "wss",
                PeerConnSource::Automatic,
                PeerConnectionOrigin::Network,
                false,
            )
            .await;
            let wss_id = wss.get_conn_id();
            peer.add_peer_conn(wss).await.unwrap();
            let (udp, _udp_remote) = handshaken_connection(
                context,
                "udp",
                PeerConnSource::Automatic,
                PeerConnectionOrigin::Network,
                false,
            )
            .await;
            let udp_id = udp.get_conn_id();
            peer.add_peer_conn(udp).await.unwrap();
            assert!(peer.conns.contains_key(&wss_id));
            assert!(peer.conns.contains_key(&udp_id));
            assert!(peer.has_connection_at_least_as_preferred("udp", false));
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
    async fn attached_ring_does_not_satisfy_network_transport_preference() {
        let context: ArcPeerContext = Arc::new(NoopPeerContext::default());
        let (local_tx, _local_rx) = create_packet_recv_chan();
        let peer = Peer::new(2, local_tx, context.clone());
        let (conn, _remote) = handshaken_connection(
            context,
            "ring",
            PeerConnSource::Automatic,
            PeerConnectionOrigin::Attached,
            false,
        )
        .await;
        peer.add_peer_conn(conn).await.unwrap();
        assert!(!peer.has_connection_at_least_as_preferred("udp", false));
        assert!(!peer.has_connection_at_least_as_preferred("http3", true));
    }
}
