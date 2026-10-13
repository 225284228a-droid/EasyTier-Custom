use std::{collections::BTreeSet, net::IpAddr, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::runtime::CoreRuntimeConfigStore,
    gateway::magic_dns::{MagicDnsRouteSnapshot, MagicDnsRouteSource},
    gateway::proxy::cidr_monitor::{ProxyCidrDiff, collect_proxy_cidr_diff},
    host::packet::HostPacket,
    peers::peer_manager::PeerManagerCore,
};

#[cfg(feature = "proxy-packet")]
use crate::foundation::stats::{LabelSet, LabelType, MetricName};
#[cfg(feature = "proxy-packet")]
use crate::gateway::magic_dns::{
    MagicDnsQueryResolver, MagicDnsResolverRegistration, magic_dns_packet_filter,
};
#[cfg(feature = "proxy-packet")]
use crate::gateway::udp_broadcast::UdpBroadcastRelayStats;

use super::packet_io::parse_ip_packet;

/// Stable packet- and route-plane projection for platform integrations.
pub struct CorePacketPlane {
    peer_manager: Arc<PeerManagerCore>,
    runtime_config: CoreRuntimeConfigStore,
    proxy_cidr_monitor_available: bool,
    #[cfg(feature = "proxy-smoltcp-stack")]
    pub(super) dns_egress: std::sync::OnceLock<Arc<dyn super::vpn_dns::VpnDnsSocketFactory>>,
}

impl CorePacketPlane {
    pub async fn vpn_route_exclusions(&self) -> crate::config::vpn::routes::VpnRouteExclusions {
        self.peer_manager.vpn_route_exclusions().await
    }

    pub async fn vpn_route_plan(
        &self,
        dns_servers: Vec<IpAddr>,
    ) -> (crate::config::vpn::routes::VpnRoutePlan, u64) {
        let exclusions = self.vpn_route_exclusions().await;
        let engine = self.peer_manager.vpn_engine();
        engine.update_route_context(exclusions, dns_servers);
        engine.route_plan()
    }

    pub fn resolve_vpn(
        &self,
        destination: &IpAddr,
        bytes: u32,
    ) -> crate::peers::vpn_policy::VpnDecision {
        self.peer_manager.vpn_engine().resolve(destination, bytes)
    }

    /// Route/source preflight checks do not count as transmitted packets.
    pub fn classify_vpn(&self, destination: &IpAddr) -> crate::peers::vpn_policy::VpnDecision {
        self.peer_manager.vpn_engine().resolve(destination, 0)
    }

    pub fn vpn_overlay_destination(&self, destination: &IpAddr) -> bool {
        self.peer_manager
            .vpn_engine()
            .is_overlay_destination(destination)
    }

    pub fn update_vpn_dns_servers(&self, servers: Vec<IpAddr>) {
        self.peer_manager.vpn_engine().update_dns_servers(servers);
    }
    pub fn update_vpn_system_dns_servers(&self, servers: Vec<IpAddr>) {
        self.peer_manager
            .vpn_engine()
            .update_system_dns_servers(servers);
    }
    pub(super) fn new(
        peer_manager: Arc<PeerManagerCore>,
        runtime_config: CoreRuntimeConfigStore,
        proxy_cidr_monitor_available: bool,
    ) -> Self {
        Self {
            peer_manager,
            runtime_config,
            proxy_cidr_monitor_available,
            #[cfg(feature = "proxy-smoltcp-stack")]
            dns_egress: std::sync::OnceLock::new(),
        }
    }

    #[cfg(feature = "proxy-smoltcp-stack")]
    pub async fn vpn_dns_tcp_connect(
        &self,
        address: std::net::SocketAddr,
        timeout: std::time::Duration,
    ) -> std::io::Result<crate::gateway::DataPlaneTcpStream> {
        self.dns_egress
            .get()
            .ok_or_else(|| std::io::Error::other("DNS packet plane unavailable"))?
            .tcp_connect(address, timeout)
            .await
    }

    #[cfg(feature = "proxy-smoltcp-stack")]
    pub async fn vpn_dns_udp_bind(
        &self,
        address: std::net::SocketAddr,
        timeout: std::time::Duration,
    ) -> std::io::Result<crate::gateway::DataPlaneUdpSocket> {
        self.dns_egress
            .get()
            .ok_or_else(|| std::io::Error::other("DNS packet plane unavailable"))?
            .udp_bind(address, timeout)
            .await
    }

    pub async fn send_ip_packet(&self, packet: HostPacket) -> anyhow::Result<()> {
        let meta = parse_ip_packet(packet.payload())?;
        let source_is_local = self.peer_manager.is_local_virtual_ip(&meta.source);
        if matches!(meta.source, IpAddr::V6(ip) if ip.is_unicast_link_local()) && !source_is_local {
            return Ok(());
        }
        self.peer_manager
            .send_msg_by_ip(packet.into_core_packet(), meta.destination, source_is_local)
            .await
            .map_err(Into::into)
    }

    pub async fn send_local_ip_packet(&self, packet: HostPacket) -> anyhow::Result<()> {
        let destination = parse_ip_packet(packet.payload())?.destination;
        self.peer_manager
            .send_msg_by_ip(packet.into_core_packet(), destination, true)
            .await
            .map_err(Into::into)
    }

    pub async fn proxy_cidr_diff(
        &self,
        previous: &BTreeSet<cidr::Ipv4Cidr>,
    ) -> Option<ProxyCidrDiff> {
        if !self.proxy_cidr_monitor_available {
            return None;
        }
        Some(
            collect_proxy_cidr_diff(self.peer_manager.as_ref(), &self.runtime_config, previous)
                .await,
        )
    }

    #[cfg(feature = "proxy-packet")]
    pub fn udp_broadcast_relay_stats(&self) -> UdpBroadcastRelayStats {
        let network_name = self
            .runtime_config
            .snapshot()
            .peer
            .runtime
            .network_identity
            .network_name
            .clone();
        let labels = LabelSet::new().with_label_type(LabelType::NetworkName(network_name));
        let stats = self.peer_manager.stats_manager();
        UdpBroadcastRelayStats::new(
            stats.get_counter(MetricName::UdpBroadcastRelayPacketsCaptured, labels.clone()),
            stats.get_counter(MetricName::UdpBroadcastRelayPacketsIgnored, labels.clone()),
            stats.get_counter(
                MetricName::UdpBroadcastRelayPacketsForwarded,
                labels.clone(),
            ),
            stats.get_counter(MetricName::UdpBroadcastRelayPacketsForwardFailed, labels),
        )
    }

    #[cfg(feature = "proxy-packet")]
    pub async fn register_magic_dns_resolver(
        &self,
        fake_ip: std::net::Ipv4Addr,
        resolver: Arc<dyn MagicDnsQueryResolver>,
    ) -> MagicDnsResolverRegistration {
        let runtime = tokio::runtime::Handle::current();
        let pipeline = self
            .peer_manager
            .add_managed_nic_packet_process_pipeline(magic_dns_packet_filter(
                fake_ip,
                self.peer_manager.my_peer_id(),
                resolver,
            ))
            .await;
        MagicDnsResolverRegistration::new(Arc::downgrade(&self.peer_manager), pipeline, runtime)
    }
}

#[async_trait]
impl MagicDnsRouteSource for CorePacketPlane {
    async fn snapshot(&self) -> MagicDnsRouteSnapshot {
        MagicDnsRouteSource::snapshot(self.peer_manager.as_ref()).await
    }

    async fn revision(&self) -> quanta::Instant {
        MagicDnsRouteSource::revision(self.peer_manager.as_ref()).await
    }
}
