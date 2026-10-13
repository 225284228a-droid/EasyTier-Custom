use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use dashmap::{DashMap, mapref::entry::Entry};
use smoltcp::wire::{IPV6_HEADER_LEN, IpAddress, IpProtocol, Ipv4Packet, Ipv6Packet, UdpPacket};

use crate::{
    config::PeerId,
    packet::{PacketType, ZCPacket},
};

use super::{
    cidr_table::ProxyCidrTable,
    ip_reassembler::{ComposeIpv4PacketArgs, IpReassembler, compose_ipv4_packet},
    traits::UdpProxyRuntime,
};

#[derive(Debug, Hash, Eq, PartialEq, Clone, Copy)]
pub struct UdpNatKey {
    pub src_socket: SocketAddr,
    pub dst_socket: SocketAddr,
}

impl UdpNatKey {
    pub fn new(src_socket: SocketAddr, dst_socket: SocketAddr) -> Self {
        Self {
            src_socket,
            dst_socket,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct UdpNatEntryId(uuid::Uuid);

impl UdpNatEntryId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for UdpNatEntryId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct UdpNatEntry {
    id: UdpNatEntryId,
    src_peer_id: PeerId,
    my_peer_id: PeerId,
    src_socket: SocketAddr,
    real_dst_ip: IpAddr,
    mapped_dst_ip: IpAddr,
    /// The address the reply source is rewritten to when the OS socket sees
    /// a loopback response (the destination was this node's own IP).
    virtual_ip: IpAddr,
    last_active_time: parking_lot::Mutex<Instant>,
    stopped: AtomicBool,
    denied: bool,
}

impl UdpNatEntry {
    fn new(
        src_peer_id: PeerId,
        my_peer_id: PeerId,
        src_socket: SocketAddr,
        real_dst_ip: IpAddr,
        mapped_dst_ip: IpAddr,
        virtual_ip: IpAddr,
        denied: bool,
    ) -> Self {
        Self {
            id: UdpNatEntryId::new(),
            src_peer_id,
            my_peer_id,
            src_socket,
            real_dst_ip,
            mapped_dst_ip,
            virtual_ip,
            last_active_time: parking_lot::Mutex::new(Instant::now()),
            stopped: AtomicBool::new(false),
            denied,
        }
    }

    pub fn id(&self) -> UdpNatEntryId {
        self.id
    }

    pub fn is_denied(&self) -> bool {
        self.denied
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    fn mark_active(&self) {
        *self.last_active_time.lock() = Instant::now();
    }

    fn is_active(&self, ttl: Duration) -> bool {
        self.last_active_time.lock().elapsed() < ttl && !self.is_stopped()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct UdpProxyPeerContext {
    pub virtual_ipv4: Option<Ipv4Addr>,
    pub vpn_exit: bool,
    pub no_tun: bool,
}

#[derive(Debug)]
pub enum UdpProxyAction {
    ForwardToSocket {
        entry_id: UdpNatEntryId,
        dst: SocketAddr,
        payload: Bytes,
    },
    Drop,
    Pass,
}

#[derive(Debug)]
pub struct UdpProxyEngine {
    cidr_table: Arc<ProxyCidrTable>,
    nat_table: DashMap<UdpNatKey, Arc<UdpNatEntry>>,
    nat_ids: DashMap<UdpNatEntryId, UdpNatKey>,
    ip_reassembler: IpReassembler,
    entry_ttl: Duration,
    next_ip_id: AtomicU16,
}

impl UdpProxyEngine {
    pub fn new(cidr_table: Arc<ProxyCidrTable>, fragment_timeout: Duration) -> Self {
        Self {
            cidr_table,
            nat_table: DashMap::new(),
            nat_ids: DashMap::new(),
            ip_reassembler: IpReassembler::new(fragment_timeout),
            entry_ttl: Duration::from_secs(180),
            next_ip_id: AtomicU16::new(1),
        }
    }

    pub fn entry_ids(&self) -> Vec<UdpNatEntryId> {
        self.nat_ids.iter().map(|entry| *entry.key()).collect()
    }

    pub fn remove_expired_entries(&self) -> Vec<UdpNatEntryId> {
        let mut removed = Vec::new();
        self.nat_table.retain(|_, entry| {
            if entry.is_active(self.entry_ttl) {
                true
            } else {
                tracing::info!(?entry, "udp nat table entry removed");
                entry.stop();
                self.nat_ids.remove(&entry.id());
                removed.push(entry.id());
                false
            }
        });
        self.nat_table.shrink_to_fit();
        self.nat_ids.shrink_to_fit();
        removed
    }

    pub fn remove_entry(&self, entry_id: UdpNatEntryId) {
        if let Some((_, key)) = self.nat_ids.remove(&entry_id)
            && let Some((_, entry)) = self.nat_table.remove(&key)
        {
            entry.stop();
        }
    }

    pub fn remove_expired_fragments(&self) {
        self.ip_reassembler.remove_expired_packets();
    }

    pub fn handle_peer_packet(
        &self,
        packet: &ZCPacket,
        ctx: UdpProxyPeerContext,
        runtime: &impl UdpProxyRuntime,
    ) -> UdpProxyAction {
        if self.cidr_table.is_empty() && !ctx.vpn_exit && !ctx.no_tun {
            return UdpProxyAction::Pass;
        }

        let Some(hdr) = packet.peer_manager_header() else {
            return UdpProxyAction::Pass;
        };
        let is_vpn_exit = hdr.is_vpn_exit();
        if hdr.packet_type != PacketType::Data as u8 || hdr.is_no_proxy() {
            return UdpProxyAction::Pass;
        };

        // The version nibble dispatches before the v4-only gates below: a
        // v6-only exit node has no virtual IPv4 but still relays v6.
        if packet.payload().first().is_some_and(|byte| byte >> 4 == 6) {
            return self.handle_peer_packet_v6(packet, ctx, runtime, is_vpn_exit);
        }

        let Some(virtual_ipv4) = ctx.virtual_ipv4 else {
            return UdpProxyAction::Pass;
        };

        let Ok(ipv4) = Ipv4Packet::new_checked(packet.payload()) else {
            return UdpProxyAction::Pass;
        };
        if ipv4.version() != 4 || ipv4.next_header() != IpProtocol::Udp {
            return UdpProxyAction::Pass;
        }

        let origin_dst_ip = ipv4.dst_addr();
        let mut real_dst_ip = origin_dst_ip;
        let no_tun_local_virtual_ip =
            ctx.no_tun && Some(origin_dst_ip) == ctx.virtual_ipv4.as_ref().copied();
        if let Some(mapped_real_ip) = self.cidr_table.lookup_v4(origin_dst_ip) {
            real_dst_ip = mapped_real_ip;
        } else if !is_vpn_exit && !no_tun_local_virtual_ip {
            return UdpProxyAction::Pass;
        }

        let reassembled_buf;
        let udp_packet = if IpReassembler::is_packet_fragmented(&ipv4) {
            let Some(buf) = self.ip_reassembler.add_fragment(&ipv4) else {
                return UdpProxyAction::Drop;
            };
            reassembled_buf = buf;
            let Ok(udp_packet) = UdpPacket::new_checked(reassembled_buf.as_slice()) else {
                return UdpProxyAction::Pass;
            };
            udp_packet
        } else {
            let Ok(udp_packet) = UdpPacket::new_checked(ipv4.payload()) else {
                return UdpProxyAction::Pass;
            };
            udp_packet
        };

        let dst_socket = if runtime.is_ip_local_virtual_ip(&IpAddr::V4(real_dst_ip)) {
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), udp_packet.dst_port())
        } else {
            SocketAddr::new(real_dst_ip.into(), udp_packet.dst_port())
        };

        tracing::trace!(
            ?packet,
            ?ipv4,
            ?udp_packet,
            "udp nat packet request received"
        );

        let nat_key = UdpNatKey::new(
            SocketAddr::new(ipv4.src_addr().into(), udp_packet.src_port()),
            SocketAddr::new(origin_dst_ip.into(), udp_packet.dst_port()),
        );
        let deny_dst = SocketAddr::new(real_dst_ip.into(), udp_packet.dst_port());
        let nat_entry = match self.nat_table.entry(nat_key) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let denied = runtime.should_deny_udp_proxy(deny_dst);
                let nat_entry = Arc::new(UdpNatEntry::new(
                    hdr.from_peer_id.get(),
                    hdr.to_peer_id.get(),
                    nat_key.src_socket,
                    IpAddr::V4(real_dst_ip),
                    IpAddr::V4(origin_dst_ip),
                    IpAddr::V4(virtual_ipv4),
                    denied,
                ));
                self.nat_ids.insert(nat_entry.id(), nat_key);
                tracing::info!(?packet, ?ipv4, ?udp_packet, "udp nat table entry created");
                entry.insert(nat_entry).clone()
            }
        };

        if nat_entry.is_denied() {
            tracing::debug!(
                dst_port = udp_packet.dst_port(),
                "dst socket is in running listeners, ignore it"
            );
            return UdpProxyAction::Drop;
        }

        nat_entry.mark_active();
        UdpProxyAction::ForwardToSocket {
            entry_id: nat_entry.id(),
            dst: dst_socket,
            payload: Bytes::copy_from_slice(udp_packet.payload()),
        }
    }

    /// Relays one IPv6 UDP datagram from a peer: VPN-exit traffic only, no
    /// v6 proxy-CIDR mapping exists. The OS socket reply is translated back
    /// by [`Self::compose_udp_ipv6_response`].
    fn handle_peer_packet_v6(
        &self,
        packet: &ZCPacket,
        ctx: UdpProxyPeerContext,
        runtime: &impl UdpProxyRuntime,
        is_vpn_exit: bool,
    ) -> UdpProxyAction {
        if !ctx.vpn_exit || !is_vpn_exit {
            return UdpProxyAction::Pass;
        }
        let Some(hdr) = packet.peer_manager_header() else {
            return UdpProxyAction::Pass;
        };
        let Ok(ipv6) = Ipv6Packet::new_checked(packet.payload()) else {
            return UdpProxyAction::Pass;
        };
        if ipv6.next_header() != IpProtocol::Udp {
            return UdpProxyAction::Pass;
        }
        let Ok(udp_packet) = UdpPacket::new_checked(ipv6.payload()) else {
            return UdpProxyAction::Pass;
        };

        let origin_dst_ip = ipv6.dst_addr();
        let real_dst_ip = origin_dst_ip;
        let dst_socket = if runtime.is_ip_local_virtual_ip(&IpAddr::V6(real_dst_ip)) {
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), udp_packet.dst_port())
        } else {
            SocketAddr::new(real_dst_ip.into(), udp_packet.dst_port())
        };

        let nat_key = UdpNatKey::new(
            SocketAddr::new(ipv6.src_addr().into(), udp_packet.src_port()),
            SocketAddr::new(origin_dst_ip.into(), udp_packet.dst_port()),
        );
        let deny_dst = SocketAddr::new(real_dst_ip.into(), udp_packet.dst_port());
        let nat_entry = match self.nat_table.entry(nat_key) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let denied = runtime.should_deny_udp_proxy(deny_dst);
                let nat_entry = Arc::new(UdpNatEntry::new(
                    hdr.from_peer_id.get(),
                    hdr.to_peer_id.get(),
                    nat_key.src_socket,
                    IpAddr::V6(real_dst_ip),
                    IpAddr::V6(origin_dst_ip),
                    IpAddr::V6(origin_dst_ip),
                    denied,
                ));
                self.nat_ids.insert(nat_entry.id(), nat_key);
                tracing::info!(
                    ?packet,
                    ?ipv6,
                    ?udp_packet,
                    "udp v6 nat table entry created"
                );
                entry.insert(nat_entry).clone()
            }
        };

        if nat_entry.is_denied() {
            tracing::debug!(
                dst_port = udp_packet.dst_port(),
                "dst socket is in running listeners, ignore it"
            );
            return UdpProxyAction::Drop;
        }

        nat_entry.mark_active();
        UdpProxyAction::ForwardToSocket {
            entry_id: nat_entry.id(),
            dst: dst_socket,
            payload: Bytes::copy_from_slice(udp_packet.payload()),
        }
    }

    pub fn handle_socket_response(
        &self,
        entry_id: UdpNatEntryId,
        src_socket: SocketAddr,
        payload: &[u8],
        ipv4_mtu: usize,
    ) -> anyhow::Result<Vec<ZCPacket>> {
        let Some(key) = self.nat_ids.get(&entry_id).map(|entry| *entry.value()) else {
            return Ok(Vec::new());
        };
        let Some(entry) = self.nat_table.get(&key).map(|entry| entry.clone()) else {
            return Ok(Vec::new());
        };
        entry.mark_active();

        if entry.real_dst_ip.is_ipv6() {
            return compose_udp_ipv6_response(&entry, src_socket, payload);
        }

        let SocketAddr::V4(mut src_v4) = src_socket else {
            return Ok(Vec::new());
        };
        let SocketAddr::V4(nat_src_v4) = entry.src_socket else {
            return Ok(Vec::new());
        };

        let has_mapped_dst = entry.real_dst_ip != entry.mapped_dst_ip;
        let mut reply_src_ip = IpAddr::V4(*src_v4.ip());
        if has_mapped_dst && reply_src_ip == entry.real_dst_ip {
            reply_src_ip = entry.mapped_dst_ip;
        } else if reply_src_ip.is_loopback() {
            reply_src_ip = entry.virtual_ip;
        }
        if has_mapped_dst && reply_src_ip == entry.real_dst_ip {
            reply_src_ip = entry.mapped_dst_ip;
        }
        let IpAddr::V4(reply_src_v4) = reply_src_ip else {
            return Ok(Vec::new());
        };
        src_v4.set_ip(reply_src_v4);

        let payload_mtu = ipv4_mtu
            .saturating_sub(smoltcp::wire::IPV4_HEADER_LEN)
            .max(8);
        let payload_mtu = payload_mtu - (payload_mtu % 8);
        let ip_id = self.next_ip_id.fetch_add(1, Ordering::Relaxed);
        compose_udp_ipv4_response(&entry, &src_v4, &nat_src_v4, payload, payload_mtu, ip_id)
    }
}

fn compose_udp_ipv4_response(
    entry: &UdpNatEntry,
    src_v4: &SocketAddrV4,
    nat_src_v4: &SocketAddrV4,
    payload: &[u8],
    payload_mtu: usize,
    ip_id: u16,
) -> anyhow::Result<Vec<ZCPacket>> {
    assert_eq!(0, payload_mtu % 8);

    let mut buf =
        vec![0; smoltcp::wire::IPV4_HEADER_LEN + smoltcp::wire::UDP_HEADER_LEN + payload.len()];
    let udp_start = smoltcp::wire::IPV4_HEADER_LEN;
    let udp_len = smoltcp::wire::UDP_HEADER_LEN + payload.len();
    {
        let mut udp_packet = UdpPacket::new_unchecked(&mut buf[udp_start..udp_start + udp_len]);
        udp_packet.set_src_port(src_v4.port());
        udp_packet.set_dst_port(nat_src_v4.port());
        udp_packet.set_len(udp_len as u16);
        udp_packet.payload_mut().copy_from_slice(payload);
        udp_packet.fill_checksum(
            &IpAddress::Ipv4(*src_v4.ip()),
            &IpAddress::Ipv4(*nat_src_v4.ip()),
        );
    }

    let mut packets = Vec::new();
    compose_ipv4_packet(
        ComposeIpv4PacketArgs {
            buf: &mut buf,
            src_v4: src_v4.ip(),
            dst_v4: nat_src_v4.ip(),
            next_protocol: IpProtocol::Udp,
            payload_len: udp_len,
            payload_mtu,
            ip_id,
        },
        |buf| {
            let mut packet = ZCPacket::new_with_payload(buf);
            packet.fill_peer_manager_hdr(
                entry.my_peer_id,
                entry.src_peer_id,
                PacketType::Data as u8,
            );
            packet
                .mut_peer_manager_header()
                .expect("peer manager header")
                .set_no_proxy(true);
            packets.push(packet);
            Ok(())
        },
    )?;

    Ok(packets)
}

/// Builds one IPv6 UDP reply from the OS socket's datagram. IPv6 is never
/// fragmented in transit, so datagrams beyond the minimum link MTU cannot
/// enter the tunnel and are dropped instead.
fn compose_udp_ipv6_response(
    entry: &UdpNatEntry,
    src_socket: SocketAddr,
    payload: &[u8],
) -> anyhow::Result<Vec<ZCPacket>> {
    let SocketAddr::V6(nat_dst_v6) = entry.src_socket else {
        return Ok(Vec::new());
    };
    let mut reply_src_ip = src_socket.ip();
    if reply_src_ip.is_loopback() {
        reply_src_ip = entry.virtual_ip;
    }
    let IpAddr::V6(reply_src_v6) = reply_src_ip else {
        return Ok(Vec::new());
    };

    const IPV6_UDP_PAYLOAD_LIMIT: usize = 1280 - IPV6_HEADER_LEN - smoltcp::wire::UDP_HEADER_LEN;
    if payload.len() > IPV6_UDP_PAYLOAD_LIMIT {
        tracing::warn!(
            len = payload.len(),
            limit = IPV6_UDP_PAYLOAD_LIMIT,
            "udp v6 response exceeds the ipv6 minimum mtu, dropped"
        );
        return Ok(Vec::new());
    }

    let udp_len = smoltcp::wire::UDP_HEADER_LEN + payload.len();
    let mut buf = vec![0; IPV6_HEADER_LEN + udp_len];
    {
        let mut udp_packet = UdpPacket::new_unchecked(&mut buf[IPV6_HEADER_LEN..]);
        udp_packet.set_src_port(src_socket.port());
        udp_packet.set_dst_port(nat_dst_v6.port());
        udp_packet.set_len(udp_len as u16);
        udp_packet.payload_mut().copy_from_slice(payload);
        udp_packet.fill_checksum(
            &IpAddress::Ipv6(reply_src_v6),
            &IpAddress::Ipv6(*nat_dst_v6.ip()),
        );
    }
    {
        let mut ipv6_packet = Ipv6Packet::new_unchecked(&mut buf);
        // The buffer starts zeroed, so the version nibble must be stamped
        // explicitly or the reply is not a valid IPv6 packet. smoltcp's
        // new_checked does not validate the version, so this is invisible
        // to the unit tests below without the explicit assertions.
        ipv6_packet.set_version(6);
        ipv6_packet.set_traffic_class(0);
        ipv6_packet.set_flow_label(0);
        ipv6_packet.set_payload_len(udp_len as u16);
        ipv6_packet.set_next_header(IpProtocol::Udp);
        ipv6_packet.set_hop_limit(64);
        ipv6_packet.set_src_addr(reply_src_v6);
        ipv6_packet.set_dst_addr(*nat_dst_v6.ip());
    }

    let mut packet = ZCPacket::new_with_payload(&buf);
    packet.fill_peer_manager_hdr(entry.my_peer_id, entry.src_peer_id, PacketType::Data as u8);
    packet
        .mut_peer_manager_header()
        .expect("peer manager header")
        .set_no_proxy(true);
    Ok(vec![packet])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::proxy::{
        cidr_table::{ProxyCidrRule, ProxyCidrSnapshot},
        traits::{ProxyRuntimeError, ProxyRuntimeInfo, ProxyRuntimeSnapshot, UdpProxyResponseSink},
    };
    use std::net::SocketAddrV6;

    struct TestRuntime;

    impl ProxyRuntimeInfo for TestRuntime {
        fn proxy_runtime_snapshot(&self) -> ProxyRuntimeSnapshot {
            ProxyRuntimeSnapshot::default()
        }

        fn is_ip_local_virtual_ip(&self, ip: &IpAddr) -> bool {
            matches!(ip, IpAddr::V4(ip) if *ip == Ipv4Addr::new(10, 144, 144, 204))
        }
    }

    #[async_trait::async_trait]
    impl UdpProxyRuntime for TestRuntime {
        fn should_deny_udp_proxy(&self, _dst_socket: SocketAddr) -> bool {
            false
        }

        fn udp_response_ipv4_mtu(&self) -> usize {
            1280
        }

        async fn send_udp_to_socket(
            &self,
            _entry_id: UdpNatEntryId,
            _dst: SocketAddr,
            _payload: bytes::Bytes,
            _response_sink: std::sync::Weak<dyn UdpProxyResponseSink>,
        ) -> Result<(), ProxyRuntimeError> {
            Ok(())
        }

        fn close_udp_socket(&self, _entry_id: UdpNatEntryId) {}
    }

    #[test]
    fn socket_response_uses_mapped_source_for_mapped_destination() {
        let table = Arc::new(ProxyCidrTable::from_snapshot(ProxyCidrSnapshot {
            rules: vec![ProxyCidrRule {
                cidr: "10.144.144.204/32".parse().unwrap(),
                mapped_cidr: Some("10.10.10.3/32".parse().unwrap()),
            }],
        }));
        let engine = UdpProxyEngine::new(table, Duration::from_secs(10));

        let mut request =
            vec![0; smoltcp::wire::IPV4_HEADER_LEN + smoltcp::wire::UDP_HEADER_LEN + 7];
        {
            let mut ipv4 = Ipv4Packet::new_unchecked(&mut request);
            ipv4.set_version(4);
            ipv4.set_header_len(smoltcp::wire::IPV4_HEADER_LEN as u8);
            ipv4.set_total_len(
                (smoltcp::wire::IPV4_HEADER_LEN + smoltcp::wire::UDP_HEADER_LEN + 7) as u16,
            );
            ipv4.set_hop_limit(64);
            ipv4.set_next_header(IpProtocol::Udp);
            ipv4.set_src_addr("10.144.144.206".parse().unwrap());
            ipv4.set_dst_addr("10.10.10.3".parse().unwrap());
            ipv4.fill_checksum();
        }
        {
            let mut udp = UdpPacket::new_unchecked(&mut request[smoltcp::wire::IPV4_HEADER_LEN..]);
            udp.set_src_port(53864);
            udp.set_dst_port(12345);
            udp.set_len((smoltcp::wire::UDP_HEADER_LEN + 7) as u16);
            udp.payload_mut().copy_from_slice(b"request");
            udp.fill_checksum(
                &IpAddress::Ipv4("10.144.144.206".parse().unwrap()),
                &IpAddress::Ipv4("10.10.10.3".parse().unwrap()),
            );
        }

        let mut zc = ZCPacket::new_with_payload(&request);
        zc.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);

        let action = engine.handle_peer_packet(
            &zc,
            UdpProxyPeerContext {
                virtual_ipv4: Some("10.144.144.204".parse().unwrap()),
                vpn_exit: false,
                no_tun: false,
            },
            &TestRuntime,
        );
        let UdpProxyAction::ForwardToSocket { entry_id, .. } = action else {
            panic!("expected forward action");
        };

        let packets = engine
            .handle_socket_response(entry_id, "127.0.0.1:12345".parse().unwrap(), b"reply", 1280)
            .unwrap();
        assert_eq!(packets.len(), 1);
        let ipv4 = Ipv4Packet::new_checked(packets[0].payload()).unwrap();
        assert_eq!(ipv4.src_addr(), Ipv4Addr::new(10, 10, 10, 3));
        assert_eq!(ipv4.dst_addr(), Ipv4Addr::new(10, 144, 144, 206));
        let udp = UdpPacket::new_checked(ipv4.payload()).unwrap();
        assert_eq!(udp.src_port(), 12345);
        assert_eq!(udp.dst_port(), 53864);
        assert_eq!(udp.payload(), b"reply");
    }

    fn empty_cidr() -> Arc<ProxyCidrTable> {
        Arc::new(ProxyCidrTable::from_snapshot(ProxyCidrSnapshot {
            rules: vec![],
        }))
    }

    fn build_udp_v6_packet(src: SocketAddrV6, dst: SocketAddrV6, payload: &[u8]) -> ZCPacket {
        let mut raw =
            vec![0; smoltcp::wire::IPV6_HEADER_LEN + smoltcp::wire::UDP_HEADER_LEN + payload.len()];
        {
            let mut udp = UdpPacket::new_unchecked(&mut raw[smoltcp::wire::IPV6_HEADER_LEN..]);
            udp.set_src_port(src.port());
            udp.set_dst_port(dst.port());
            udp.set_len((smoltcp::wire::UDP_HEADER_LEN + payload.len()) as u16);
            udp.payload_mut().copy_from_slice(payload);
            udp.fill_checksum(&IpAddress::Ipv6(*src.ip()), &IpAddress::Ipv6(*dst.ip()));
        }
        {
            let mut ipv6 = Ipv6Packet::new_unchecked(&mut raw);
            ipv6.set_version(6);
            ipv6.set_traffic_class(0);
            ipv6.set_flow_label(0);
            ipv6.set_payload_len((smoltcp::wire::UDP_HEADER_LEN + payload.len()) as u16);
            ipv6.set_next_header(IpProtocol::Udp);
            ipv6.set_hop_limit(64);
            ipv6.set_src_addr(*src.ip());
            ipv6.set_dst_addr(*dst.ip());
        }

        let mut packet = ZCPacket::new_with_payload(&raw);
        packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        packet
    }

    fn v6_exit_ctx() -> UdpProxyPeerContext {
        UdpProxyPeerContext {
            virtual_ipv4: Some("10.144.144.204".parse().unwrap()),
            vpn_exit: true,
            no_tun: false,
        }
    }

    #[test]
    fn udp_v6_exit_request_forwards_to_socket() {
        let engine = UdpProxyEngine::new(empty_cidr(), Duration::from_secs(10));
        let client: SocketAddrV6 = "[fd00:abcd::1]:53864".parse().unwrap();
        let internet: SocketAddrV6 = "[2606:4700:4700::1111]:53".parse().unwrap();
        let mut packet = build_udp_v6_packet(client, internet, b"query");
        packet.mut_peer_manager_header().unwrap().set_vpn_exit(true);

        let action = engine.handle_peer_packet(&packet, v6_exit_ctx(), &TestRuntime);
        let UdpProxyAction::ForwardToSocket {
            entry_id,
            dst,
            payload,
        } = action
        else {
            panic!("expected forward action, got {action:?}");
        };
        assert_eq!(dst, SocketAddr::from(internet));
        assert_eq!(payload.as_ref(), b"query");

        let reply = engine
            .handle_socket_response(
                entry_id,
                "[2606:4700:4700::1111]:53".parse().unwrap(),
                b"answer",
                1280,
            )
            .unwrap();
        assert_eq!(reply.len(), 1);
        let ip = Ipv6Packet::new_checked(reply[0].payload()).unwrap();
        assert_eq!(ip.version(), 6);
        assert_eq!(ip.src_addr(), *internet.ip());
        assert_eq!(ip.dst_addr(), *client.ip());
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert_eq!(udp.src_port(), internet.port());
        assert_eq!(udp.dst_port(), client.port());
        assert_eq!(udp.payload(), b"answer");
        assert!(reply[0].peer_manager_header().unwrap().is_no_proxy());
    }

    #[test]
    fn udp_v6_non_exit_traffic_passes_through() {
        let engine = UdpProxyEngine::new(empty_cidr(), Duration::from_secs(10));
        let client: SocketAddrV6 = "[fd00:abcd::1]:53864".parse().unwrap();
        let internet: SocketAddrV6 = "[2606:4700:4700::1111]:53".parse().unwrap();
        let client_ctx = UdpProxyPeerContext {
            vpn_exit: false,
            ..v6_exit_ctx()
        };

        // without ctx.vpn_exit nothing is relayed, whatever the header says
        let mut packet = build_udp_v6_packet(client, internet, b"query");
        let action = engine.handle_peer_packet(&packet, client_ctx, &TestRuntime);
        assert!(matches!(action, UdpProxyAction::Pass));
        assert!(engine.nat_table.is_empty());

        packet.mut_peer_manager_header().unwrap().set_vpn_exit(true);
        let action = engine.handle_peer_packet(&packet, client_ctx, &TestRuntime);
        assert!(matches!(action, UdpProxyAction::Pass));
        assert!(engine.nat_table.is_empty());
    }
}
