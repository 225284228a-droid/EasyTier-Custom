//! Negotiated, hop-local packet striping. The original network packet remains
//! intact, including encryption/compression flags, inside the session envelope.
use crate::{
    config::PeerId,
    packet::{PEER_MANAGER_HEADER_SIZE, PacketType, ZCPacket, ZCPacketType},
    peers::{
        PacketRecvChan, PeerPacketIngress, send_peer_packet_to_chan,
        traffic_metrics::is_relay_data_packet_type,
    },
};
use bytes::BytesMut;
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) const FEATURE_PREFIX: &str = "p2p-parallel-v1:";
#[cfg(test)]
pub(crate) const OVERHEAD: usize = crate::packet::PARALLEL_PACKET_OVERHEAD;
const MAX_PACKETS: usize = 8192;
// Permit a timed resynchronization after a long loss burst while rejecting
// absurd near-u64 sequence numbers before they can poison the live window.
const MAX_SEQUENCE_GAP: u64 = (MAX_PACKETS * MAX_PACKETS) as u64;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const GLOBAL_MAX_BYTES: usize = 128 * 1024 * 1024;
static BUFFERED_BYTES: AtomicUsize = AtomicUsize::new(0);

fn reserve_budget(budget: &AtomicUsize, bytes: usize, limit: usize) -> bool {
    budget
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes).filter(|total| *total <= limit)
        })
        .is_ok()
}

pub(crate) fn tx_epoch(features: &[String]) -> Option<uuid::Uuid> {
    let mut offers = features.iter().filter(|f| f.starts_with("p2p-tx-"));
    let epoch = offers
        .next()?
        .strip_prefix("p2p-tx-v1:")?
        .parse::<uuid::Uuid>()
        .ok()?;
    if epoch.is_nil() || offers.next().is_some() {
        None
    } else {
        Some(epoch)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParallelMemberToken {
    pub pool_id: uuid::Uuid,
    pub epoch: uuid::Uuid,
    pub slot: u8,
}
tokio::task_local! { pub(crate) static DIAL_MEMBER: ParallelMemberToken; }
impl ParallelMemberToken {
    pub(crate) fn feature(&self) -> String {
        format!("p2p-pool-v1:{}:{}:{}", self.pool_id, self.epoch, self.slot)
    }
    pub(crate) fn from_features(features: &[String]) -> Option<Self> {
        let mut offers = features.iter().filter(|f| f.starts_with("p2p-pool-"));
        let mut fields = offers.next()?.strip_prefix("p2p-pool-v1:")?.split(':');
        let pool_id = fields.next()?.parse::<uuid::Uuid>().ok()?;
        let epoch = fields.next()?.parse::<uuid::Uuid>().ok()?;
        let slot = fields.next()?.parse::<u8>().ok()?;
        if pool_id.is_nil()
            || epoch.is_nil()
            || slot > 15
            || fields.next().is_some()
            || offers.next().is_some()
        {
            return None;
        }
        Some(Self {
            pool_id,
            epoch,
            slot,
        })
    }
}
#[derive(Default)]
pub(crate) struct ParallelReservations(Mutex<BTreeSet<u8>>);
impl ParallelReservations {
    pub(crate) fn len(&self) -> usize {
        self.0.lock().len()
    }
    pub(crate) fn reserve(
        self: &Arc<Self>,
        pool_id: uuid::Uuid,
        epoch: uuid::Uuid,
        target: u32,
        occupied: &BTreeSet<u8>,
    ) -> Option<ParallelDialReservation> {
        let mut reserved = self.0.lock();
        if reserved.len() >= 2 {
            return None;
        }
        let slot =
            (0..target.min(16) as u8).find(|s| !occupied.contains(s) && !reserved.contains(s))?;
        reserved.insert(slot);
        Some(ParallelDialReservation {
            token: ParallelMemberToken {
                pool_id,
                epoch,
                slot,
            },
            reservations: self.clone(),
            node_permit: None,
        })
    }
}
pub(crate) struct ParallelDialReservation {
    token: ParallelMemberToken,
    reservations: Arc<ParallelReservations>,
    node_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl ParallelDialReservation {
    pub(crate) fn with_node_permit(
        mut self,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Self {
        self.node_permit = permit;
        self
    }
    #[cfg(test)]
    pub(crate) fn token(&self) -> ParallelMemberToken {
        self.token.clone()
    }
    pub(crate) async fn scope<F: std::future::Future>(&self, future: F) -> F::Output {
        DIAL_MEMBER.scope(self.token.clone(), future).await
    }
}
impl Drop for ParallelDialReservation {
    fn drop(&mut self) {
        self.reservations.0.lock().remove(&self.token.slot);
    }
}

pub(crate) fn inherit_scope<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    let token = DIAL_MEMBER.try_with(Clone::clone).ok();
    scope_optional(token, future)
}
pub(crate) async fn scope_optional<F: std::future::Future>(
    token: Option<ParallelMemberToken>,
    future: F,
) -> F::Output {
    match token {
        Some(token) => DIAL_MEMBER.scope(token, future).await,
        None => future.await,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParallelOffer {
    pub count: u32,
    pub disabled: bool,
    pub limit: u32,
}
impl ParallelOffer {
    pub(crate) fn local(context: &dyn crate::peers::context::PeerContext) -> Self {
        Self {
            count: context.parallel_connections().clamp(1, 16),
            disabled: context.disable_parallel_connections(),
            limit: context.parallel_connection_limit().clamp(1, 16),
        }
    }
    pub(crate) fn feature(self) -> String {
        format!(
            "{FEATURE_PREFIX}{}:{}:{}",
            self.count,
            u8::from(self.disabled),
            self.limit
        )
    }
    fn bytes(self) -> [u8; 4] {
        [
            1,
            self.count as u8,
            u8::from(self.disabled),
            self.limit as u8,
        ]
    }
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 4
            || bytes[0] != 1
            || !(1..=16).contains(&bytes[1])
            || bytes[2] > 1
            || !(1..=16).contains(&bytes[3])
        {
            return None;
        };
        Some(Self {
            count: bytes[1] as u32,
            disabled: bytes[2] != 0,
            limit: bytes[3] as u32,
        })
    }
    pub(crate) fn target(self, remote: Self) -> u32 {
        if self.disabled || remote.disabled {
            1
        } else {
            (self.count + remote.count)
                .div_ceil(2)
                .min(self.limit)
                .min(remote.limit)
        }
    }
}
#[cfg(test)]
pub(crate) fn feature(count: u32, disabled: bool) -> String {
    ParallelOffer {
        count: count.clamp(1, 16),
        disabled,
        limit: 16,
    }
    .feature()
}

/// Fail closed for unknown versions, duplicate offers and malformed values.
pub(crate) fn remote_offer(features: &[String]) -> Option<ParallelOffer> {
    let mut offers = features.iter().filter(|f| f.starts_with("p2p-parallel-"));
    let mut fields = offers.next()?.strip_prefix(FEATURE_PREFIX)?.split(':');
    let count = fields.next()?.parse::<u32>().ok()?;
    let disabled = match fields.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let limit = fields.next()?.parse::<u32>().ok()?;
    if !(1..=16).contains(&count)
        || !(1..=16).contains(&limit)
        || fields.next().is_some()
        || offers.next().is_some()
    {
        return None;
    }
    Some(ParallelOffer {
        count,
        disabled,
        limit,
    })
}

#[cfg(test)]
pub(crate) fn negotiated_target(local: u32, disabled: bool, features: &[String]) -> u32 {
    remote_offer(features).map_or(1, |remote| {
        ParallelOffer {
            count: local.clamp(1, 16),
            disabled,
            limit: 16,
        }
        .target(remote)
    })
}

#[derive(Default)]
struct PolicyState {
    local: Option<ParallelOffer>,
    remote: Option<ParallelOffer>,
    ack: Option<ParallelOffer>,
    changed: Option<Instant>,
    remote_anchor: Option<(uuid::Uuid, uuid::Uuid)>,
}
#[derive(Default)]
pub(crate) struct ParallelPolicy(Mutex<PolicyState>);
impl ParallelPolicy {
    pub(crate) fn remote(&self) -> Option<ParallelOffer> {
        self.0.lock().remote
    }
    pub(crate) fn handshake(&self, offer: Option<ParallelOffer>) {
        if offer.is_some() {
            let mut s = self.0.lock();
            s.remote = offer;
            s.ack = None;
            s.changed = Some(Instant::now());
        }
    }
    pub(crate) fn control_payload(
        &self,
        local: ParallelOffer,
        anchor: Option<(uuid::Uuid, uuid::Uuid)>,
    ) -> [u8; 40] {
        let mut s = self.0.lock();
        if s.local != Some(local) {
            s.local = Some(local);
            s.changed = Some(Instant::now());
        }
        let mut bytes = [0; 40];
        bytes[..4].copy_from_slice(&local.bytes());
        if let Some(remote) = s.remote {
            bytes[4..8].copy_from_slice(&remote.bytes())
        };
        if let Some((pool, epoch)) = anchor {
            bytes[8..24].copy_from_slice(pool.as_bytes());
            bytes[24..40].copy_from_slice(epoch.as_bytes());
        }
        bytes
    }
    pub(crate) fn receive_control(&self, bytes: &[u8]) {
        if bytes.len() != 40 {
            return;
        };
        let Some(remote) = ParallelOffer::from_bytes(&bytes[..4]) else {
            return;
        };
        let ack = ParallelOffer::from_bytes(&bytes[4..8]);
        let anchor = uuid::Uuid::from_slice(&bytes[8..24])
            .ok()
            .zip(uuid::Uuid::from_slice(&bytes[24..40]).ok())
            .filter(|(pool, epoch)| !pool.is_nil() && !epoch.is_nil());
        let mut s = self.0.lock();
        if s.remote != Some(remote) {
            s.changed = Some(Instant::now());
        }
        s.remote = Some(remote);
        s.ack = ack;
        s.remote_anchor = anchor;
    }
    pub(crate) fn remote_anchor(&self) -> Option<(uuid::Uuid, uuid::Uuid)> {
        self.0.lock().remote_anchor
    }
    pub(crate) fn can_trim(&self, local: ParallelOffer) -> bool {
        let s = self.0.lock();
        s.local == Some(local)
            && s.ack == Some(local)
            && s.changed
                .is_some_and(|t| t.elapsed() >= Duration::from_millis(250))
    }
    pub(crate) fn reset(&self) {
        *self.0.lock() = PolicyState::default();
    }
}

pub(crate) fn stripeable(packet: &ZCPacket) -> bool {
    packet
        .peer_manager_header()
        .is_some_and(|h| is_relay_data_packet_type(h.packet_type))
}

pub(crate) fn wrap(
    packet: ZCPacket,
    from: PeerId,
    to: PeerId,
    epoch: uuid::Uuid,
    seq: u64,
) -> ZCPacket {
    let packet_type = if packet.is_lossy() {
        PacketType::ParallelLossyData
    } else {
        PacketType::ParallelData
    };
    let latency_first = packet
        .peer_manager_header()
        .is_some_and(|h| h.is_latency_first());
    let mut payload = Vec::with_capacity(24 + packet.tunnel_payload().len());
    payload.extend_from_slice(epoch.as_bytes());
    payload.extend_from_slice(&seq.to_be_bytes());
    payload.extend_from_slice(packet.tunnel_payload());
    let mut out = ZCPacket::new_with_payload(&payload);
    out.fill_peer_manager_hdr(from, to, packet_type as u8);
    out.mut_peer_manager_header()
        .unwrap()
        .set_latency_first(latency_first);
    out
}

/// Decoding happens after the per-connection session filter and before traffic
/// limits or ACL dispatch. An envelope may carry data only, never a control RPC.
pub(crate) fn unwrap(
    packet: ZCPacket,
    from: PeerId,
    to: PeerId,
) -> Option<(uuid::Uuid, u64, ZCPacket)> {
    let hdr = packet.peer_manager_header()?;
    let lossy = hdr.packet_type == PacketType::ParallelLossyData as u8;
    if (!lossy && hdr.packet_type != PacketType::ParallelData as u8)
        || hdr.from_peer_id.get() != from
        || hdr.to_peer_id.get() != to
        || hdr.is_encrypted()
    {
        return None;
    }
    let payload = packet.payload();
    if payload.len() < 24 + PEER_MANAGER_HEADER_SIZE {
        return None;
    }
    let epoch = uuid::Uuid::from_slice(&payload[..16]).ok()?;
    if epoch.is_nil() {
        return None;
    }
    let seq = u64::from_be_bytes(payload[16..24].try_into().ok()?);
    let inner = ZCPacket::new_from_buf(BytesMut::from(&payload[24..]), ZCPacketType::DummyTunnel);
    // The original length describes the network packet before encryption;
    // AEAD tails and compression deliberately change the physical length.
    if !stripeable(&inner) || inner.is_lossy() != lossy {
        return None;
    }
    Some((epoch, seq, inner))
}

struct Buffered {
    packet: Option<ZCPacket>,
    ingress: PeerPacketIngress,
    bytes: usize,
    arrived: Instant,
}
impl Drop for Buffered {
    fn drop(&mut self) {
        BUFFERED_BYTES.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}
#[derive(Default)]
struct ReorderState {
    epoch: Option<uuid::Uuid>,
    authorized: VecDeque<uuid::Uuid>,
    next: u64,
    packets: BTreeMap<u64, Buffered>,
    bytes: usize,
    gap_since: Option<Instant>,
}
impl ReorderState {
    fn clear(&mut self) {
        self.packets.clear();
        self.bytes = 0;
        self.gap_since = None;
    }
    fn drain(&mut self, skip_gap: bool) -> Vec<Buffered> {
        if skip_gap && let Some(first) = self.packets.keys().next() {
            self.next = *first;
        }
        let mut out = Vec::new();
        while let Some(packet) = self.packets.remove(&self.next) {
            self.next = self.next.saturating_add(1);
            self.bytes -= packet.bytes;
            out.push(packet);
        }
        self.gap_since = if self.packets.is_empty() {
            None
        } else if out.is_empty() {
            self.gap_since.or_else(|| {
                self.packets
                    .first_key_value()
                    .map(|(_, packet)| packet.arrived)
            })
        } else {
            self.packets.values().map(|packet| packet.arrived).min()
        };
        out
    }
}
impl Drop for ReorderState {
    fn drop(&mut self) {
        self.clear();
    }
}

pub(crate) struct ParallelReceiver {
    state: Mutex<ReorderState>,
    delivery: tokio::sync::Mutex<()>,
    sender: PacketRecvChan,
    gap_ms: AtomicUsize,
}
impl ParallelReceiver {
    pub(crate) fn new(sender: PacketRecvChan) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ReorderState::default()),
            delivery: tokio::sync::Mutex::new(()),
            sender,
            gap_ms: AtomicUsize::new(20),
        })
    }
    pub(crate) fn set_max_rtt_us(&self, rtt: u64) {
        self.gap_ms.store(
            (rtt.saturating_mul(3) / 1000).clamp(20, 250) as usize,
            Ordering::Relaxed,
        );
    }
    /// Called only after the last authenticated connection disappears. A live
    /// peer cannot rotate epochs by injecting a different data envelope.
    pub(crate) fn reset_epoch(&self) {
        let mut state = self.state.lock();
        state.clear();
        state.epoch = None;
        state.authorized.clear();
        state.next = 0;
    }
    pub(crate) fn authorize_epoch(&self, epoch: uuid::Uuid) {
        let mut state = self.state.lock();
        if state.epoch != Some(epoch) && !state.authorized.contains(&epoch) {
            state.authorized.push_back(epoch);
            if state.authorized.len() > 16 {
                state.authorized.pop_front();
            }
        }
    }
    async fn deliver(&self, packets: Vec<Buffered>) {
        for mut p in packets {
            if send_peer_packet_to_chan(&self.sender, p.packet.take().unwrap(), p.ingress)
                .await
                .is_err()
            {
                break;
            }
        }
    }
    pub(crate) async fn receive(
        &self,
        epoch: uuid::Uuid,
        seq: u64,
        packet: ZCPacket,
        ingress: PeerPacketIngress,
    ) {
        let _delivery = self.delivery.lock().await;
        let packets = {
            let mut s = self.state.lock();
            if s.epoch != Some(epoch) {
                let Some(position) = s.authorized.iter().position(|allowed| *allowed == epoch)
                else {
                    return;
                };
                s.authorized.remove(position);
                s.clear();
                s.epoch = Some(epoch);
                s.next = 0;
            }
            if seq == u64::MAX
                || seq < s.next
                || seq.saturating_sub(s.next) > MAX_SEQUENCE_GAP
                || s.packets.contains_key(&seq)
            {
                return;
            }
            let bytes = packet.buf_len();
            if bytes > MAX_BYTES {
                return;
            }
            let mut ready = Vec::new();
            while s.packets.len() >= MAX_PACKETS || s.bytes.saturating_add(bytes) > MAX_BYTES {
                ready.extend(s.drain(true));
            }
            // Capacity recovery can move `next` beyond this arrival while
            // releasing older buffered packets. Never reinsert a late packet
            // behind that new frontier or a later gap flush could rewind it.
            if seq >= s.next && reserve_budget(&BUFFERED_BYTES, bytes, GLOBAL_MAX_BYTES) {
                s.bytes += bytes;
                s.packets.insert(
                    seq,
                    Buffered {
                        packet: Some(packet),
                        ingress,
                        bytes,
                        arrived: Instant::now(),
                    },
                );
                ready.extend(s.drain(false));
            }
            ready
        };
        self.deliver(packets).await;
    }
    pub(crate) async fn flush_expired(&self) {
        let _delivery = self.delivery.lock().await;
        let packets = {
            let mut s = self.state.lock();
            if s.gap_since.is_none_or(|t| {
                t.elapsed() < Duration::from_millis(self.gap_ms.load(Ordering::Relaxed) as u64)
            }) {
                return;
            }
            let mut ready = Vec::new();
            while s.gap_since.is_some_and(|t| {
                t.elapsed() >= Duration::from_millis(self.gap_ms.load(Ordering::Relaxed) as u64)
            }) {
                ready.extend(s.drain(true));
            }
            ready
        };
        self.deliver(packets).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn data(payload: &[u8]) -> ZCPacket {
        let mut packet = ZCPacket::new_with_payload(payload);
        packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        packet
    }

    #[test]
    fn global_budget_enforces_128_mib_releases_and_rejects_overflow() {
        let budget = AtomicUsize::new(0);
        for _ in 0..8 {
            assert!(reserve_budget(&budget, MAX_BYTES, GLOBAL_MAX_BYTES));
        }
        assert!(!reserve_budget(&budget, 1, GLOBAL_MAX_BYTES));
        assert!(!reserve_budget(&budget, usize::MAX, GLOBAL_MAX_BYTES));
        budget.fetch_sub(MAX_BYTES, Ordering::Relaxed);
        assert!(reserve_budget(&budget, MAX_BYTES, GLOBAL_MAX_BYTES));
        assert_eq!(budget.load(Ordering::Relaxed), 128 * 1024 * 1024);
    }

    #[tokio::test]
    async fn direction_capacity_skips_gaps_at_8192_packets_or_16_mib() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let consumer = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let _consumer = tokio_util::task::AbortOnDropHandle::new(consumer);
        tokio::time::timeout(Duration::from_secs(5), async {
            let receiver = ParallelReceiver::new(tx);
            let epoch = uuid::Uuid::new_v4();
            receiver.authorize_epoch(epoch);
            let ingress = PeerPacketIngress::Peer {
                peer_id: 1,
                conn_id: uuid::Uuid::new_v4(),
                origin: crate::peers::PeerConnectionOrigin::Direct,
            };
            for seq in 1..=MAX_PACKETS as u64 {
                receiver.receive(epoch, seq, data(b""), ingress).await;
            }
            assert_eq!(receiver.state.lock().packets.len(), MAX_PACKETS);
            receiver
                .receive(epoch, 0, data(b"late during capacity recovery"), ingress)
                .await;
            assert!(receiver.state.lock().packets.is_empty());
            assert_eq!(receiver.state.lock().next, MAX_PACKETS as u64 + 1);
            receiver.flush_expired().await;
            assert_eq!(receiver.state.lock().next, MAX_PACKETS as u64 + 1);
            let fresh = uuid::Uuid::new_v4();
            receiver.authorize_epoch(fresh);
            let payload = vec![0; MAX_BYTES / 2];
            receiver.receive(fresh, 1, data(&payload), ingress).await;
            receiver.receive(fresh, 3, data(&payload), ingress).await;
            assert!(receiver.state.lock().bytes <= MAX_BYTES);
            assert_eq!(receiver.state.lock().packets.len(), 1);
        })
        .await
        .expect("capacity checks must keep draining the bounded delivery channel");
    }
    #[test]
    fn offers_are_strict_and_symmetric() {
        for (left, right, want) in [(1, 1, 1), (2, 3, 3), (16, 16, 16), (1, 5, 3)] {
            assert_eq!(
                negotiated_target(left, false, &[feature(right, false)]),
                want
            );
            assert_eq!(
                negotiated_target(right, false, &[feature(left, false)]),
                want
            );
            assert_eq!(negotiated_target(left, true, &[feature(right, false)]), 1);
            assert_eq!(negotiated_target(left, false, &[feature(right, true)]), 1);
        }
        let ordinary = ParallelOffer {
            count: 16,
            disabled: false,
            limit: 16,
        };
        let foreign = ParallelOffer {
            count: 16,
            disabled: false,
            limit: 3,
        };
        assert_eq!(ordinary.target(foreign), 3);
        assert_eq!(foreign.target(ordinary), 3);
        assert_eq!(negotiated_target(1, false, &[feature(5, false)]), 3);
        assert_eq!(negotiated_target(5, false, &[feature(1, false)]), 3);
        assert_eq!(negotiated_target(16, true, &[feature(16, false)]), 1);
        assert_eq!(
            negotiated_target(16, false, &[feature(5, false), feature(5, false)]),
            1
        );
        assert_eq!(
            negotiated_target(16, false, &["p2p-parallel-v2:5:0".into()]),
            1
        );
        assert_eq!(negotiated_target(16, false, &[]), 1);
    }
    #[test]
    fn envelope_preserves_network_packet() {
        let mut packet = ZCPacket::new_with_payload(b"network encrypted payload");
        packet.fill_peer_manager_hdr(7, 9, PacketType::Data as u8);
        packet
            .mut_peer_manager_header()
            .unwrap()
            .set_encrypted(true);
        packet.mut_inner().extend_from_slice(&[0x5a; 28]);
        let original = packet.tunnel_payload().to_vec();
        let epoch = uuid::Uuid::new_v4();
        let wire = wrap(packet, 1, 2, epoch, 123);
        assert_eq!(wire.tunnel_payload().len(), original.len() + OVERHEAD);
        let (actual_epoch, seq, inner) = unwrap(wire, 1, 2).unwrap();
        assert_eq!((actual_epoch, seq), (epoch, 123));
        assert_eq!(inner.tunnel_payload(), original);
    }
    #[tokio::test]
    async fn reorders_across_connections_and_drops_duplicates() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let receiver = ParallelReceiver::new(tx);
        let epoch = uuid::Uuid::new_v4();
        receiver.authorize_epoch(epoch);
        let ingress = PeerPacketIngress::Peer {
            peer_id: 1,
            conn_id: uuid::Uuid::new_v4(),
            origin: crate::peers::PeerConnectionOrigin::Direct,
        };
        receiver
            .receive(epoch, 1, ZCPacket::new_with_payload(b"one"), ingress)
            .await;
        receiver
            .receive(epoch, 0, ZCPacket::new_with_payload(b"zero"), ingress)
            .await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"zero");
        assert_eq!(rx.recv().await.unwrap().payload(), b"one");
        receiver
            .receive(epoch, 0, ZCPacket::new_with_payload(b"duplicate"), ingress)
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(5), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn loss_beyond_one_window_recovers_and_huge_sequence_cannot_poison_it() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let receiver = ParallelReceiver::new(tx);
        let epoch = uuid::Uuid::new_v4();
        receiver.authorize_epoch(epoch);
        let ingress = PeerPacketIngress::Peer {
            peer_id: 1,
            conn_id: uuid::Uuid::new_v4(),
            origin: crate::peers::PeerConnectionOrigin::Direct,
        };
        receiver
            .receive(
                epoch,
                u64::MAX - 1,
                ZCPacket::new_with_payload(b"bad"),
                ingress,
            )
            .await;
        receiver
            .receive(
                epoch,
                MAX_PACKETS as u64,
                ZCPacket::new_with_payload(b"recovered"),
                ingress,
            )
            .await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        receiver.flush_expired().await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"recovered");
        receiver
            .receive(
                epoch,
                MAX_PACKETS as u64 + 1,
                ZCPacket::new_with_payload(b"next"),
                ingress,
            )
            .await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"next");
    }

    #[tokio::test]
    async fn every_expired_gap_flushes_and_only_handshake_authorized_epochs_can_replace() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let receiver = ParallelReceiver::new(tx);
        let epoch = uuid::Uuid::new_v4();
        receiver.authorize_epoch(epoch);
        let ingress = PeerPacketIngress::Peer {
            peer_id: 1,
            conn_id: uuid::Uuid::new_v4(),
            origin: crate::peers::PeerConnectionOrigin::Direct,
        };
        for seq in [1u64, 3, 5] {
            receiver
                .receive(
                    epoch,
                    seq,
                    ZCPacket::new_with_payload(&seq.to_be_bytes()),
                    ingress,
                )
                .await;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        receiver.flush_expired().await;
        for seq in [1u64, 3, 5] {
            assert_eq!(rx.recv().await.unwrap().payload(), seq.to_be_bytes());
        }
        let fresh = uuid::Uuid::new_v4();
        receiver
            .receive(
                fresh,
                0,
                ZCPacket::new_with_payload(b"unauthorized"),
                ingress,
            )
            .await;
        assert_eq!(receiver.state.lock().epoch, Some(epoch));
        receiver.authorize_epoch(fresh);
        receiver
            .receive(fresh, 0, ZCPacket::new_with_payload(b"fresh"), ingress)
            .await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"fresh");
        receiver
            .receive(epoch, 6, ZCPacket::new_with_payload(b"retired"), ingress)
            .await;
        assert_eq!(receiver.state.lock().epoch, Some(fresh));
    }
}
