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
use std::collections::{BTreeSet, HashMap};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

/// Version 2 envelopes chain each packet to the previous one of its flow, and
/// receivers order per flow; a version 1 peer stays on one connection.
pub(crate) const FEATURE_PREFIX: &str = "p2p-parallel-v2:";
/// Epoch, sequence and the distance back to the flow's previous packet.
const ENVELOPE_SIZE: usize = 28;
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

/// Runs one automatic dial in the peer's parallel pool. A peer whose pool
/// (`pool_target > 1`) has no free slot for the dial's protocol is skipped:
/// the dial is not started and `None` is returned.
pub(crate) async fn dial_in_slot<F: std::future::Future>(
    reservation: Option<ParallelDialReservation>,
    pool_target: u32,
    dial: F,
) -> Option<F::Output> {
    match reservation {
        Some(reservation) => Some(reservation.scope(dial).await),
        None if pool_target > 1 => None,
        None => Some(dial.await),
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

/// `prev` is how far back the previous packet of the same flow is, or zero
/// when the flow starts here.
pub(crate) fn wrap(
    packet: ZCPacket,
    from: PeerId,
    to: PeerId,
    epoch: uuid::Uuid,
    seq: u64,
    prev: u32,
) -> ZCPacket {
    let packet_type = if packet.is_lossy() {
        PacketType::ParallelLossyData
    } else {
        PacketType::ParallelData
    };
    let latency_first = packet
        .peer_manager_header()
        .is_some_and(|h| h.is_latency_first());
    let mut payload = Vec::with_capacity(ENVELOPE_SIZE + packet.tunnel_payload().len());
    payload.extend_from_slice(epoch.as_bytes());
    payload.extend_from_slice(&seq.to_be_bytes());
    payload.extend_from_slice(&prev.to_be_bytes());
    payload.extend_from_slice(packet.tunnel_payload());
    let mut out = ZCPacket::new_with_payload(&payload);
    out.fill_peer_manager_hdr(from, to, packet_type as u8);
    out.mut_peer_manager_header()
        .unwrap()
        .set_latency_first(latency_first);
    // Local metadata: lets a multi-stream tunnel keep the flow on one stream.
    out.flow_key = packet.flow_key;
    out
}

/// Decoding happens after the per-connection session filter and before traffic
/// limits or ACL dispatch. An envelope may carry data only, never a control RPC.
pub(crate) fn unwrap(
    packet: ZCPacket,
    from: PeerId,
    to: PeerId,
) -> Option<(uuid::Uuid, u64, Option<u64>, ZCPacket)> {
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
    if payload.len() < ENVELOPE_SIZE + PEER_MANAGER_HEADER_SIZE {
        return None;
    }
    let epoch = uuid::Uuid::from_slice(&payload[..16]).ok()?;
    if epoch.is_nil() {
        return None;
    }
    let seq = u64::from_be_bytes(payload[16..24].try_into().ok()?);
    let prev = match u32::from_be_bytes(payload[24..28].try_into().ok()?) {
        0 => None,
        distance => Some(seq.checked_sub(u64::from(distance))?),
    };
    let inner = ZCPacket::new_from_buf(
        BytesMut::from(&payload[ENVELOPE_SIZE..]),
        ZCPacketType::DummyTunnel,
    );
    // The original length describes the network packet before encryption;
    // AEAD tails and compression deliberately change the physical length.
    if !stripeable(&inner) || inner.is_lossy() != lossy {
        return None;
    }
    Some((epoch, seq, prev, inner))
}

struct Buffered {
    packet: Option<ZCPacket>,
    ingress: PeerPacketIngress,
    /// Bytes reserved from the shared budget; zero for a packet that never waits.
    bytes: usize,
    arrived: Instant,
}
impl Drop for Buffered {
    fn drop(&mut self) {
        BUFFERED_BYTES.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// A lane that has carried nothing for this long no longer bounds which
/// sequences may still be in flight.
const LANE_IDLE: Duration = Duration::from_secs(1);

/// One member connection as seen by the receiver.
struct Lane {
    /// The transport delivers a connection's packets in the order sent.
    ordered: bool,
    /// Highest sequence this lane carried in the current epoch.
    frontier: Option<u64>,
    /// Registration or the latest arrival.
    last: Instant,
}

/// Whether a transport delivers one connection's packets in order: single
/// stream transports do. Datagram transports may reorder, and an HTTP3 tunnel
/// spreads flows over several streams of its connection.
pub(crate) fn ordered_transport(tunnel_type: &str) -> bool {
    matches!(
        tunnel_type.rsplit('-').next().unwrap_or(tunnel_type),
        "tcp" | "ws" | "wss" | "quic"
    )
}

/// Sequences tracked for duplicates and for which predecessors were handed
/// on. Anything older counts as settled.
const WINDOW: u64 = 1 << 16;
const _: () = assert!(WINDOW as usize > MAX_PACKETS);

struct SequenceWindow {
    low: u64,
    received: Vec<u64>,
    delivered: Vec<u64>,
}
impl Default for SequenceWindow {
    fn default() -> Self {
        Self {
            low: 0,
            received: vec![0; WINDOW as usize / 64],
            delivered: vec![0; WINDOW as usize / 64],
        }
    }
}
impl SequenceWindow {
    fn bit(seq: u64) -> (usize, u64) {
        let slot = (seq % WINDOW) as usize;
        (slot / 64, 1 << (slot % 64))
    }
    fn test(bits: &[u64], seq: u64) -> bool {
        let (word, mask) = Self::bit(seq);
        bits[word] & mask != 0
    }
    fn set(bits: &mut [u64], seq: u64) {
        let (word, mask) = Self::bit(seq);
        bits[word] |= mask;
    }
    fn received(&self, seq: u64) -> bool {
        seq < self.low || Self::test(&self.received, seq)
    }
    fn mark_received(&mut self, seq: u64) {
        if seq >= self.low {
            Self::set(&mut self.received, seq);
        }
    }
    /// A predecessor is satisfied once it was handed on or given up on.
    fn settled(&self, seq: u64) -> bool {
        seq < self.low || Self::test(&self.delivered, seq)
    }
    fn mark_settled(&mut self, seq: u64) {
        if seq >= self.low {
            Self::set(&mut self.received, seq);
            Self::set(&mut self.delivered, seq);
        }
    }
    /// Its successor no longer waits for it; should it still turn up, it is
    /// handed on late rather than dropped.
    fn mark_given_up(&mut self, seq: u64) {
        if seq >= self.low {
            Self::set(&mut self.delivered, seq);
        }
    }
    fn reset(&mut self) {
        self.low = 0;
        self.received.fill(0);
        self.delivered.fill(0);
    }
    /// Forgets everything below `low`, freeing its ring slots.
    fn advance(&mut self, low: u64) {
        if low <= self.low {
            return;
        }
        if low - self.low >= WINDOW {
            self.received.fill(0);
            self.delivered.fill(0);
        } else {
            for seq in self.low..low {
                let (word, mask) = Self::bit(seq);
                self.received[word] &= !mask;
                self.delivered[word] &= !mask;
            }
        }
        self.low = low;
    }
}

struct Waiting {
    /// The flow's previous packet, which must be handed on first.
    prev: u64,
    buffered: Buffered,
}

/// Puts striped packets back in order per flow. Each packet names the previous
/// packet of its flow, so a missing packet holds back only its own flow; the
/// sender keeps flows apart from local metadata that is never on the wire.
#[derive(Default)]
struct ReorderState {
    epoch: Option<uuid::Uuid>,
    authorized: VecDeque<uuid::Uuid>,
    window: SequenceWindow,
    /// Packets whose predecessor has not been handed on, by sequence.
    packets: BTreeMap<u64, Waiting>,
    /// The waiting packet for each predecessor, by predecessor.
    waiting_on: BTreeMap<u64, u64>,
    bytes: usize,
    lanes: HashMap<uuid::Uuid, Lane>,
    /// Delivery tickets issued so far; batches are delivered in ticket order.
    issued: u64,
}
impl ReorderState {
    fn clear(&mut self) {
        self.packets.clear();
        self.waiting_on.clear();
        self.bytes = 0;
        self.window.reset();
        // Every lane may carry anything of a new epoch until it is seen.
        let now = Instant::now();
        for lane in self.lanes.values_mut() {
            lane.frontier = None;
            lane.last = now;
        }
    }
    fn satisfied(&self, prev: Option<u64>) -> bool {
        prev.is_none_or(|prev| self.window.settled(prev))
    }
    /// Hands on `seq` and every packet of its flow that waited behind it.
    fn deliver_chain(&mut self, mut seq: u64, first: Buffered, out: &mut Vec<Buffered>) {
        self.window.mark_settled(seq);
        out.push(first);
        while let Some(next) = self.waiting_on.remove(&seq) {
            let Some(waiting) = self.packets.remove(&next) else {
                break;
            };
            self.bytes -= waiting.buffered.bytes;
            self.window.mark_settled(next);
            out.push(waiting.buffered);
            seq = next;
        }
    }
    /// Gives up on the predecessor of waiting packet `seq` and hands it on.
    fn release(&mut self, seq: u64, out: &mut Vec<Buffered>) {
        let Some(waiting) = self.packets.remove(&seq) else {
            return;
        };
        self.waiting_on.remove(&waiting.prev);
        self.bytes -= waiting.buffered.bytes;
        self.window.mark_given_up(waiting.prev);
        self.deliver_chain(seq, waiting.buffered, out);
    }
    /// Moves the window up so `seq` fits; whatever waited below it or behind
    /// a predecessor below it is handed on in sequence order.
    fn make_room(&mut self, seq: u64, out: &mut Vec<Buffered>) {
        if seq < self.window.low + WINDOW {
            return;
        }
        let low = seq + 1 - WINDOW;
        let stale: Vec<u64> = self
            .packets
            .iter()
            .filter(|(seq, waiting)| **seq < low || waiting.prev < low)
            .map(|(seq, _)| *seq)
            .collect();
        for seq in stale {
            self.release(seq, out);
        }
        self.window.advance(low);
    }
    /// The lowest frontier of the active lanes, when every active lane is
    /// ordered and has been seen: a sequence still missing at or below it was
    /// lost, not delayed. A lane that has not been seen may carry anything.
    fn ordered_frontier(&self, now: Instant) -> Option<u64> {
        let mut lowest = None;
        for lane in self.lanes.values() {
            if now.saturating_duration_since(lane.last) > LANE_IDLE {
                continue;
            }
            let frontier = lane.frontier.filter(|_| lane.ordered)?;
            lowest = Some(lowest.map_or(frontier, |lowest: u64| lowest.min(frontier)));
        }
        lowest
    }
    /// Whether every active lane is ordered. A gap behind ordered lanes is a
    /// delay, not a loss: loss on them shows up as a passed frontier or as a
    /// removed lane, so the reorder timer is only a backstop for a stalled one.
    fn all_lanes_ordered(&self, now: Instant) -> bool {
        let mut active = self
            .lanes
            .values()
            .filter(|lane| now.saturating_duration_since(lane.last) <= LANE_IDLE)
            .peekable();
        active.peek().is_some() && active.all(|lane| lane.ordered)
    }
    /// Hands on packets whose predecessor every ordered lane has passed
    /// without carrying it, instead of waiting for the reorder timer.
    fn skip_lost(&mut self, now: Instant) -> Vec<Buffered> {
        let mut out = Vec::new();
        let Some(frontier) = self.ordered_frontier(now) else {
            return out;
        };
        let lost: Vec<u64> = self
            .waiting_on
            .range(..=frontier)
            .filter(|(prev, _)| !self.window.received(**prev))
            .map(|(_, seq)| *seq)
            .collect();
        for seq in lost {
            self.release(seq, &mut out);
        }
        out
    }
}
impl Drop for ReorderState {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Default)]
struct DeliveryTurns {
    served: u64,
    /// Tickets whose holder was cancelled before its turn.
    abandoned: BTreeSet<u64>,
    /// When the latest batch finished. A lane blocked behind delivery cannot
    /// read its next packet, so that time does not count towards a gap.
    last_done: Option<Instant>,
}

/// Completes a delivery ticket when dropped, including when the receiving
/// task is cancelled, so a closed connection cannot stall later batches.
struct Turn<'a> {
    receiver: &'a ParallelReceiver,
    ticket: u64,
}
impl Drop for Turn<'_> {
    fn drop(&mut self) {
        {
            let mut turns = self.receiver.turns.lock();
            turns.last_done = Some(Instant::now());
            if turns.served == self.ticket {
                turns.served += 1;
                loop {
                    let served = turns.served;
                    if !turns.abandoned.remove(&served) {
                        break;
                    }
                    turns.served += 1;
                }
            } else {
                turns.abandoned.insert(self.ticket);
            }
        }
        self.receiver.turn_changed.notify_waiters();
    }
}

pub(crate) struct ParallelReceiver {
    state: Mutex<ReorderState>,
    turns: Mutex<DeliveryTurns>,
    turn_changed: tokio::sync::Notify,
    /// Signalled when a gap starts buffering packets; flushing has nothing to
    /// do while the buffer is empty.
    buffered: tokio::sync::Notify,
    sender: PacketRecvChan,
    gap_ms: AtomicUsize,
}
impl ParallelReceiver {
    pub(crate) fn new(sender: PacketRecvChan) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ReorderState::default()),
            turns: Mutex::new(DeliveryTurns::default()),
            turn_changed: tokio::sync::Notify::new(),
            buffered: tokio::sync::Notify::new(),
            sender,
            gap_ms: AtomicUsize::new(20),
        })
    }
    #[cfg(test)]
    pub(crate) fn state_summary(&self) -> String {
        let s = self.state.lock();
        format!(
            "epoch {:?}, low {}, waiting {:?}, lanes {}",
            s.epoch,
            s.window.low,
            s.packets
                .iter()
                .map(|(seq, w)| (*seq, w.prev))
                .collect::<Vec<_>>(),
            s.lanes.len()
        )
    }
    pub(crate) fn set_max_rtt_us(&self, rtt: u64) {
        self.gap_ms.store(
            (rtt.saturating_mul(3) / 1000).clamp(20, 250) as usize,
            Ordering::Relaxed,
        );
    }
    /// Registers a member connection. Only lanes registered as ordered let
    /// the receiver skip a gap before the reorder timer expires.
    pub(crate) fn register_lane(&self, conn_id: uuid::Uuid, ordered: bool) {
        self.state.lock().lanes.insert(
            conn_id,
            Lane {
                ordered,
                frontier: None,
                last: Instant::now(),
            },
        );
    }
    pub(crate) fn forget_lane(&self, conn_id: &uuid::Uuid) {
        self.state.lock().lanes.remove(conn_id);
    }
    /// Called only after the last authenticated connection disappears. A live
    /// peer cannot rotate epochs by injecting a different data envelope.
    pub(crate) fn reset_epoch(&self) {
        let mut state = self.state.lock();
        state.clear();
        state.lanes.clear();
        state.epoch = None;
        state.authorized.clear();
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
    /// Delivers a drained batch after every earlier batch. Only batches with
    /// deliverable packets wait here; buffering never waits for delivery.
    async fn deliver(&self, ticket: u64, packets: Vec<Buffered>) {
        let _turn = Turn {
            receiver: self,
            ticket,
        };
        loop {
            let notified = self.turn_changed.notified();
            if self.turns.lock().served == ticket {
                break;
            }
            notified.await;
        }
        for mut p in packets {
            if send_peer_packet_to_chan(&self.sender, p.packet.take().unwrap(), p.ingress)
                .await
                .is_err()
            {
                break;
            }
        }
    }
    /// Waits until packets are buffered behind a gap.
    pub(crate) async fn wait_buffered(&self) {
        loop {
            let notified = self.buffered.notified();
            if !self.state.lock().packets.is_empty() {
                return;
            }
            notified.await;
        }
    }
    fn issue(state: &mut ReorderState, ready: Vec<Buffered>) -> Option<(u64, Vec<Buffered>)> {
        if ready.is_empty() {
            return None;
        }
        let ticket = state.issued;
        state.issued += 1;
        Some((ticket, ready))
    }
    pub(crate) async fn receive(
        &self,
        epoch: uuid::Uuid,
        seq: u64,
        prev: Option<u64>,
        packet: ZCPacket,
        ingress: PeerPacketIngress,
    ) {
        let (batch, started_buffering) = {
            let mut s = self.state.lock();
            if s.epoch != Some(epoch) {
                let Some(position) = s.authorized.iter().position(|allowed| *allowed == epoch)
                else {
                    return;
                };
                s.authorized.remove(position);
                s.clear();
                s.epoch = Some(epoch);
            }
            if seq == u64::MAX
                || seq.saturating_sub(s.window.low) > MAX_SEQUENCE_GAP
                || prev.is_some_and(|prev| prev >= seq)
            {
                return;
            }
            let was_empty = s.packets.is_empty();
            let now = Instant::now();
            if let PeerPacketIngress::Peer { conn_id, .. } = ingress {
                // An unregistered connection counts as unordered.
                let lane = s.lanes.entry(conn_id).or_insert(Lane {
                    ordered: false,
                    frontier: None,
                    last: now,
                });
                lane.frontier = Some(lane.frontier.map_or(seq, |frontier| frontier.max(seq)));
                lane.last = now;
            }
            let mut ready = Vec::new();
            // Duplicates and packets older than the window are dropped, but
            // they still advance their lane's frontier.
            if !s.window.received(seq) {
                s.make_room(seq, &mut ready);
                s.window.mark_received(seq);
                let bytes = packet.buf_len();
                if !s.satisfied(prev) && bytes <= MAX_BYTES {
                    while s.packets.len() >= MAX_PACKETS
                        || s.bytes.saturating_add(bytes) > MAX_BYTES
                    {
                        let Some(&oldest) = s.packets.keys().next() else {
                            break;
                        };
                        s.release(oldest, &mut ready);
                    }
                }
                let waiting = |bytes| Buffered {
                    packet: Some(packet),
                    ingress,
                    bytes,
                    arrived: now,
                };
                match prev {
                    // Capacity recovery may have handed on the predecessor.
                    _ if s.satisfied(prev) => s.deliver_chain(seq, waiting(0), &mut ready),
                    Some(prev)
                        if bytes <= MAX_BYTES
                            && reserve_budget(&BUFFERED_BYTES, bytes, GLOBAL_MAX_BYTES) =>
                    {
                        s.bytes += bytes;
                        s.waiting_on.insert(prev, seq);
                        s.packets.insert(
                            seq,
                            Waiting {
                                prev,
                                buffered: waiting(bytes),
                            },
                        );
                    }
                    _ => {}
                }
            }
            ready.extend(s.skip_lost(now));
            (
                Self::issue(&mut s, ready),
                was_empty && !s.packets.is_empty(),
            )
        };
        if started_buffering {
            self.buffered.notify_waiters();
        }
        if let Some((ticket, packets)) = batch {
            self.deliver(ticket, packets).await;
        }
    }
    pub(crate) async fn flush_expired(&self) {
        let batch = {
            let mut s = self.state.lock();
            let now = Instant::now();
            // A removed lane can resolve a gap without any new arrival.
            let mut ready = s.skip_lost(now);
            let gap = Duration::from_millis(self.gap_ms.load(Ordering::Relaxed) as u64);
            let gap = if s.all_lanes_ordered(now) {
                gap.max(LANE_IDLE)
            } else {
                gap
            };
            let delivering = {
                let turns = self.turns.lock();
                turns.served != s.issued || turns.last_done.is_some_and(|t| t.elapsed() < gap)
            };
            if !delivering {
                let expired: Vec<u64> = s
                    .packets
                    .iter()
                    .filter(|(_, waiting)| {
                        now.saturating_duration_since(waiting.buffered.arrived) >= gap
                    })
                    .map(|(seq, _)| *seq)
                    .collect();
                for seq in expired {
                    s.release(seq, &mut ready);
                }
            }
            Self::issue(&mut s, ready)
        };
        if let Some((ticket, packets)) = batch {
            self.deliver(ticket, packets).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    impl ParallelReceiver {
        /// One chain, as a single flow sends it.
        async fn receive_in_order(
            &self,
            epoch: uuid::Uuid,
            seq: u64,
            packet: ZCPacket,
            ingress: PeerPacketIngress,
        ) {
            self.receive(epoch, seq, seq.checked_sub(1), packet, ingress)
                .await
        }
    }
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
                receiver
                    .receive_in_order(epoch, seq, data(b""), ingress)
                    .await;
            }
            assert_eq!(receiver.state.lock().packets.len(), MAX_PACKETS);
            receiver
                .receive_in_order(epoch, 0, data(b"late during capacity recovery"), ingress)
                .await;
            assert!(receiver.state.lock().packets.is_empty());
            assert!(receiver.state.lock().window.settled(MAX_PACKETS as u64));
            receiver.flush_expired().await;
            assert!(receiver.state.lock().packets.is_empty());
            let fresh = uuid::Uuid::new_v4();
            receiver.authorize_epoch(fresh);
            let payload = vec![0; MAX_BYTES / 2];
            receiver
                .receive_in_order(fresh, 1, data(&payload), ingress)
                .await;
            receiver
                .receive_in_order(fresh, 3, data(&payload), ingress)
                .await;
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
        packet.flow_key = Some(crate::packet::flow::FlowKey {
            src: "10.0.0.1:1".parse().unwrap(),
            dst: "10.0.0.2:2".parse().unwrap(),
            protocol: 17,
        });
        let flow = packet.flow_key;
        let wire = wrap(packet.clone(), 1, 2, epoch, 123, 5);
        assert_eq!(wire.tunnel_payload().len(), original.len() + OVERHEAD);
        assert_eq!(wire.flow_key, flow);
        let (actual_epoch, seq, prev, inner) = unwrap(wire, 1, 2).unwrap();
        assert_eq!((actual_epoch, seq, prev), (epoch, 123, Some(118)));
        assert_eq!(inner.tunnel_payload(), original);
        let start = wrap(packet.clone(), 1, 2, epoch, 7, 0);
        assert_eq!(unwrap(start, 1, 2).unwrap().2, None);
        // A predecessor before the first sequence is malformed.
        assert!(unwrap(wrap(packet, 1, 2, epoch, 3, 4), 1, 2).is_none());
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
            .receive_in_order(epoch, 1, ZCPacket::new_with_payload(b"one"), ingress)
            .await;
        receiver
            .receive_in_order(epoch, 0, ZCPacket::new_with_payload(b"zero"), ingress)
            .await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"zero");
        assert_eq!(rx.recv().await.unwrap().payload(), b"one");
        receiver
            .receive_in_order(epoch, 0, ZCPacket::new_with_payload(b"duplicate"), ingress)
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
            .receive_in_order(
                epoch,
                u64::MAX - 1,
                ZCPacket::new_with_payload(b"bad"),
                ingress,
            )
            .await;
        receiver
            .receive_in_order(
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
            .receive_in_order(
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
                .receive_in_order(
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
            .receive_in_order(
                fresh,
                0,
                ZCPacket::new_with_payload(b"unauthorized"),
                ingress,
            )
            .await;
        assert_eq!(receiver.state.lock().epoch, Some(epoch));
        receiver.authorize_epoch(fresh);
        receiver
            .receive_in_order(fresh, 0, ZCPacket::new_with_payload(b"fresh"), ingress)
            .await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"fresh");
        receiver
            .receive_in_order(epoch, 6, ZCPacket::new_with_payload(b"retired"), ingress)
            .await;
        assert_eq!(receiver.state.lock().epoch, Some(fresh));
    }

    fn lane_ingress(conn_id: uuid::Uuid) -> PeerPacketIngress {
        PeerPacketIngress::Peer {
            peer_id: 1,
            conn_id,
            origin: crate::peers::PeerConnectionOrigin::Direct,
        }
    }

    #[tokio::test]
    async fn ordered_lanes_skip_a_lost_sequence_without_waiting() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let receiver = ParallelReceiver::new(tx);
        let epoch = uuid::Uuid::new_v4();
        receiver.authorize_epoch(epoch);
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        receiver.register_lane(a, true);
        receiver.register_lane(b, true);
        receiver
            .receive_in_order(
                epoch,
                0,
                ZCPacket::new_with_payload(b"zero"),
                lane_ingress(a),
            )
            .await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"zero");
        // 1 was sent on `a`, which has already carried 3: it was lost. `b`
        // has not passed 1 yet, so it may still carry it.
        receiver
            .receive_in_order(
                epoch,
                3,
                ZCPacket::new_with_payload(b"three"),
                lane_ingress(a),
            )
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(5), rx.recv())
                .await
                .is_err()
        );
        receiver
            .receive_in_order(
                epoch,
                2,
                ZCPacket::new_with_payload(b"two"),
                lane_ingress(b),
            )
            .await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"two");
        assert_eq!(rx.recv().await.unwrap().payload(), b"three");
        assert!(receiver.state.lock().packets.is_empty());
    }

    #[tokio::test]
    async fn a_datagram_lane_keeps_waiting_for_the_reorder_timer() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let receiver = ParallelReceiver::new(tx);
        let epoch = uuid::Uuid::new_v4();
        receiver.authorize_epoch(epoch);
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        receiver.register_lane(a, true);
        receiver.register_lane(b, false);
        receiver
            .receive_in_order(
                epoch,
                1,
                ZCPacket::new_with_payload(b"one"),
                lane_ingress(b),
            )
            .await;
        receiver
            .receive_in_order(
                epoch,
                2,
                ZCPacket::new_with_payload(b"two"),
                lane_ingress(a),
            )
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(5), rx.recv())
                .await
                .is_err(),
            "a datagram lane may still carry 0"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        receiver.flush_expired().await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"one");
        assert_eq!(rx.recv().await.unwrap().payload(), b"two");
    }

    #[test]
    fn single_stream_transports_are_ordered_and_others_are_not() {
        for (tunnel_type, ordered) in [
            ("tcp", true),
            ("wss", true),
            ("http3", false),
            ("quic", true),
            ("udp", false),
            ("wg", false),
            ("ring", false),
        ] {
            assert_eq!(ordered_transport(tunnel_type), ordered, "{tunnel_type}");
        }
    }

    #[tokio::test]
    async fn a_cancelled_delivery_does_not_stall_later_batches() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let receiver = ParallelReceiver::new(tx);
        let packet = |payload: &[u8]| {
            let bytes = payload.len();
            assert!(reserve_budget(&BUFFERED_BYTES, bytes, GLOBAL_MAX_BYTES));
            Buffered {
                packet: Some(ZCPacket::new_with_payload(payload)),
                ingress: lane_ingress(uuid::Uuid::nil()),
                bytes,
                arrived: Instant::now(),
            }
        };
        let waiting = receiver.deliver(2, vec![packet(b"two")]);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut waiting)
                .await
                .is_err(),
            "ticket 2 waits for 0 and 1"
        );
        // Ticket 1 is cancelled before its turn, then 0 is delivered.
        drop(Turn {
            receiver: &receiver,
            ticket: 1,
        });
        receiver.deliver(0, vec![packet(b"zero")]).await;
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("an abandoned ticket is skipped");
        assert_eq!(rx.recv().await.unwrap().payload(), b"zero");
        assert_eq!(rx.recv().await.unwrap().payload(), b"two");
    }

    #[tokio::test]
    async fn a_gap_behind_ordered_lanes_waits_until_its_lane_is_gone() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let receiver = ParallelReceiver::new(tx);
        let epoch = uuid::Uuid::new_v4();
        receiver.authorize_epoch(epoch);
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        receiver.register_lane(a, true);
        receiver.register_lane(b, true);
        receiver
            .receive_in_order(
                epoch,
                1,
                ZCPacket::new_with_payload(b"one"),
                lane_ingress(a),
            )
            .await;
        // 0 is still on `b`: past the short reorder window it is delayed, not lost.
        tokio::time::sleep(Duration::from_millis(25)).await;
        receiver.flush_expired().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(5), rx.recv())
                .await
                .is_err()
        );
        // `b` closes with 0 queued: the remaining lane has passed it.
        receiver.forget_lane(&b);
        receiver.flush_expired().await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"one");
    }

    #[tokio::test]
    async fn a_missing_packet_holds_back_only_its_own_flow() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let receiver = ParallelReceiver::new(tx);
        let epoch = uuid::Uuid::new_v4();
        receiver.authorize_epoch(epoch);
        let ingress = lane_ingress(uuid::Uuid::new_v4());
        let send = |seq: u64, prev: Option<u64>, payload: &'static [u8]| {
            receiver.receive(
                epoch,
                seq,
                prev,
                ZCPacket::new_with_payload(payload),
                ingress,
            )
        };
        async fn quiet(rx: &mut crate::peers::PacketRecvChanReceiver) {
            assert!(
                tokio::time::timeout(Duration::from_millis(5), rx.recv())
                    .await
                    .is_err()
            );
        }
        // Flow A: 0, 2 (lost), 4. Flow B: 1, 3.
        send(0, None, b"a0").await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"a0");
        send(4, Some(2), b"a2").await;
        quiet(&mut rx).await;
        send(1, None, b"b0").await;
        send(3, Some(1), b"b1").await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"b0");
        assert_eq!(rx.recv().await.unwrap().payload(), b"b1");
        quiet(&mut rx).await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        receiver.flush_expired().await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"a2");
        // The given-up packet is handed on late should it still turn up; a
        // second copy of it is a duplicate.
        send(2, Some(0), b"a1 late").await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"a1 late");
        send(2, Some(0), b"a1 again").await;
        send(3, Some(1), b"b1 again").await;
        quiet(&mut rx).await;
        assert!(receiver.state.lock().packets.is_empty());
    }

    #[tokio::test]
    async fn a_jump_past_the_window_hands_on_what_waited_below_it() {
        let (tx, mut rx) = crate::peers::create_packet_recv_chan();
        let receiver = ParallelReceiver::new(tx);
        let epoch = uuid::Uuid::new_v4();
        receiver.authorize_epoch(epoch);
        let ingress = lane_ingress(uuid::Uuid::new_v4());
        receiver
            .receive(
                epoch,
                5,
                Some(4),
                ZCPacket::new_with_payload(b"old"),
                ingress,
            )
            .await;
        receiver
            .receive(
                epoch,
                WINDOW + 10,
                Some(9),
                ZCPacket::new_with_payload(b"new"),
                ingress,
            )
            .await;
        assert_eq!(rx.recv().await.unwrap().payload(), b"old");
        // Its predecessor is now below the window, so it counts as settled.
        assert_eq!(rx.recv().await.unwrap().payload(), b"new");
        assert!(receiver.state.lock().packets.is_empty());
    }
}
