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
    config::{PeerId, preferred_disguised_scheme},
    packet::ZCPacket,
    peers::error::Error,
    proto::{core_peer::peer::PeerConnInfo, peer_rpc::PeerIdentityType},
};
use tokio_util::task::AbortOnDropHandle;

type ArcPeerConn = Arc<PeerConn>;
type ConnMap = Arc<DashMap<PeerConnId, ArcPeerConn>>;

fn conn_latency_sort_key(latency_us: u64, is_hole_punched: bool) -> (bool, u64) {
    (is_hole_punched && latency_us == 0, latency_us)
}

fn preferred_conn_sort_key(
    latency_us: u64,
    is_hole_punched: bool,
    prefer: bool,
    disguised: bool,
    disguised_rank: u8,
) -> (bool, bool, u8, u64) {
    let (unverified, latency) = conn_latency_sort_key(latency_us, is_hole_punched);
    // `disguised_rank` orders the two disguised transports: 0 for the one that
    // matches the configured P2P transport preference (udp -> http3,
    // tcp -> wss), 1 for the other. It never reorders non-disguised paths,
    // which the `prefer && !disguised` term already handles.
    (unverified, prefer && !disguised, disguised_rank, latency)
}

fn is_disguised_tunnel_type(tunnel_type: &str) -> bool {
    tunnel_type
        .rsplit('-')
        .next()
        .is_some_and(|transport| matches!(transport, "wss" | "http3"))
}

/// The disguised transport of a connection, if it uses one. Tunnel types may
/// carry a resolution prefix (`http-txt-wss`), so only the last segment counts.
fn conn_disguised_scheme(conn: &PeerConn) -> Option<&'static str> {
    let tunnel_type = conn.get_conn_info().tunnel.as_ref()?.tunnel_type.clone();
    match tunnel_type.rsplit('-').next() {
        Some("wss") => Some("wss"),
        Some("http3") => Some("http3"),
        _ => None,
    }
}

fn conn_is_disguised(conn: &PeerConn) -> bool {
    conn_disguised_scheme(conn).is_some()
}

/// Whether a connection must be dropped once a disguised connection to the
/// same peer is up. Only the transports P2P discovered on its own are
/// redundant: peers the user configured and inbound connections are kept,
/// because dropping them would only make the other side reconnect.
fn is_redundant_once_disguised(disguised: bool, source: PeerConnSource) -> bool {
    !disguised && source == PeerConnSource::Automatic
}

pub struct Peer {
    pub peer_node_id: PeerId,
    conns: ConnMap,
    context: ArcPeerContext,

    packet_recv_chan: PacketRecvChan,

    close_event_sender: mpsc::Sender<PeerConnId>,
    #[allow(dead_code)]
    close_event_listener: AbortOnDropHandle<()>,

    shutdown_notifier: Arc<tokio::sync::Notify>,

    default_conn: Arc<ArcSwapOption<PeerConn>>,
    default_conn_update_lock: Arc<Mutex<()>>,
    peer_identity_type: Arc<AtomicCell<Option<PeerIdentityType>>>,
    peer_public_key: Arc<RwLock<Option<Vec<u8>>>>,
    #[allow(dead_code)]
    default_conn_clear_task: AbortOnDropHandle<()>,
}

impl Peer {
    pub(crate) fn new(
        peer_node_id: PeerId,
        packet_recv_chan: PacketRecvChan,
        context: ArcPeerContext,
    ) -> Self {
        let conns: ConnMap = Arc::new(DashMap::new());
        let (close_event_sender, mut close_event_receiver) = mpsc::channel(10);
        let shutdown_notifier = Arc::new(tokio::sync::Notify::new());
        let peer_identity_type = Arc::new(AtomicCell::new(None));
        let peer_identity_type_copy = peer_identity_type.clone();
        let peer_public_key = Arc::new(RwLock::new(None));
        let peer_public_key_copy = peer_public_key.clone();
        let default_conn = Arc::new(ArcSwapOption::empty());
        let default_conn_update_lock = Arc::new(Mutex::new(()));

        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let context_copy = context.clone();
        let default_conn_copy = default_conn.clone();
        let default_conn_update_lock_copy = default_conn_update_lock.clone();
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
                                let removed_conn = conns_copy.remove(&ret);
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
        let default_conn_clear_task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                crate::foundation::time::sleep(std::time::Duration::from_secs(5)).await;
                if conns_copy.len() > 1 {
                    default_conn_copy.store(None);
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
            peer_identity_type,
            peer_public_key,
            default_conn_clear_task,
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
            if let Err(e) = close_event_sender.send(conn_id).await {
                tracing::warn!(?conn_id, "failed to send close event: {}", e);
            }
        });

        self.context
            .issue_event(PeerEvent::PeerConnAdded(conn_info));

        if self.context.flags().close_redundant_conns_when_disguised {
            self.close_redundant_conns_with_disguised_peer().await;
        }
        Ok(())
    }

    /// Closes the non-disguised connections that P2P established automatically
    /// while a disguised connection to this peer is up. User-configured peers
    /// and inbound connections stay connected, because dropping them would only
    /// make the other side reconnect.
    async fn close_redundant_conns_with_disguised_peer(&self) {
        if !self
            .conns
            .iter()
            .any(|entry| !entry.value().is_closed() && conn_is_disguised(entry.value()))
        {
            return;
        }

        let redundant = self
            .conns
            .iter()
            .filter(|entry| {
                let conn = entry.value();
                !conn.is_closed()
                    && is_redundant_once_disguised(conn_is_disguised(conn), conn.conn_source())
            })
            .map(|entry| entry.value().get_conn_id())
            .collect::<Vec<_>>();

        for conn_id in redundant {
            tracing::info!(
                ?conn_id,
                peer_id = %self.peer_node_id,
                "a disguised connection is up, closing the redundant automatically established connection"
            );
            if let Err(error) = self.close_event_sender.send(conn_id).await {
                tracing::warn!(?conn_id, %error, "failed to close redundant connection");
            }
        }
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
        let prefer = !flags.disable_wss_http3_for_p2p
            && (flags.prefer_wss_http3_for_p2p || flags.only_use_wss_http3_for_hole_punching);
        let preferred_disguised = preferred_disguised_scheme(&flags.default_protocol);
        let selected = self
            .conns
            .iter()
            .filter(|conn| !conn.value().is_closed())
            .min_by_key(|conn| {
                let conn = conn.value();
                let disguised = conn_is_disguised(conn);
                let disguised_rank = if prefer && disguised && preferred_disguised.is_some() {
                    u8::from(conn_disguised_scheme(conn) != preferred_disguised)
                } else {
                    0
                };
                preferred_conn_sort_key(
                    conn.get_stats().latency_us,
                    conn.is_hole_punched(),
                    prefer,
                    disguised,
                    disguised_rank,
                )
            })
            .map(|conn| conn.value().clone());

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
        self.close_event_sender.send(*conn_id).await.unwrap();
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

    pub(crate) fn has_http3_conn(&self) -> bool {
        self.conns.iter().any(|entry| {
            !entry.value().is_closed() && conn_disguised_scheme(entry.value()) == Some("http3")
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
    use super::{conn_latency_sort_key, is_disguised_tunnel_type};

    #[test]
    fn disguise_preference_wins_after_liveness_is_verified() {
        use super::preferred_conn_sort_key as key;
        // (latency, hole_punched, prefer, disguised, disguised_rank)
        assert!(key(50_000, true, true, true, 0) < key(1_000, false, true, false, 0));
        assert!(key(1_000, false, true, false, 0) < key(0, true, true, true, 0));
        assert!(key(1_000, false, false, false, 0) < key(50_000, true, false, true, 0));
    }

    #[test]
    fn p2p_transport_preference_orders_the_disguised_transports() {
        use super::preferred_conn_sort_key as key;
        // The disguised transport matching the configured preference (rank 0)
        // wins over the other one (rank 1), regardless of the extra latency.
        assert!(key(50_000, false, true, true, 0) < key(1_000, false, true, true, 1));
        // Both disguised transports still win over raw paths.
        assert!(key(1_000, false, true, true, 1) < key(500, false, true, false, 0));
    }

    #[test]
    fn disguised_tunnel_type_detection_allows_resolution_prefixes() {
        assert!(is_disguised_tunnel_type("wss"));
        assert!(is_disguised_tunnel_type("http-txt-wss"));
        assert!(is_disguised_tunnel_type("http3"));
        assert!(!is_disguised_tunnel_type("tcp"));
        assert!(!is_disguised_tunnel_type("quic-http3-wrap"));
    }

    #[test]
    fn only_automatic_plain_conns_are_redundant_once_disguised() {
        use super::{PeerConnSource, is_redundant_once_disguised as redundant};

        // P2P-discovered plain transports are dropped while a disguised
        // connection is up.
        assert!(redundant(false, PeerConnSource::Automatic));
        // The disguised connection itself and user-configured / inbound
        // connections are always kept.
        assert!(!redundant(true, PeerConnSource::Automatic));
        assert!(!redundant(false, PeerConnSource::Manual));
        assert!(!redundant(false, PeerConnSource::Inbound));
        assert!(!redundant(true, PeerConnSource::Manual));
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
}
