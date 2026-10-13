//! Type-erased user-DNS egress. These sockets belong to the packet plane,
//! never the protected native bootstrap resolver.
use super::CoreInstanceHost;
use crate::gateway::{DataPlaneRuntime, DataPlaneTcpStream, DataPlaneUdpSocket};
use async_trait::async_trait;
use std::{
    net::SocketAddr,
    sync::{Arc, Weak},
    time::Duration,
};

#[async_trait]
pub trait VpnDnsSocketFactory: Send + Sync {
    async fn tcp_connect(
        &self,
        address: SocketAddr,
        timeout: Duration,
    ) -> std::io::Result<DataPlaneTcpStream>;
    async fn udp_bind(
        &self,
        address: SocketAddr,
        timeout: Duration,
    ) -> std::io::Result<DataPlaneUdpSocket>;
}

struct PacketDnsFactory<H: CoreInstanceHost> {
    runtime: Weak<DataPlaneRuntime<H>>,
}

#[async_trait]
impl<H: CoreInstanceHost> VpnDnsSocketFactory for PacketDnsFactory<H> {
    async fn tcp_connect(
        &self,
        address: SocketAddr,
        timeout: Duration,
    ) -> std::io::Result<DataPlaneTcpStream> {
        self.runtime
            .upgrade()
            .ok_or_else(|| std::io::Error::other("DNS packet plane stopped"))?
            .data_plane_tcp_connect(address, timeout)
            .await
            .map_err(std::io::Error::other)
    }
    async fn udp_bind(
        &self,
        address: SocketAddr,
        timeout: Duration,
    ) -> std::io::Result<DataPlaneUdpSocket> {
        self.runtime
            .upgrade()
            .ok_or_else(|| std::io::Error::other("DNS packet plane stopped"))?
            .data_plane_udp_bind_for_family(address.port(), address.is_ipv6(), timeout)
            .await
            .map_err(std::io::Error::other)
    }
}

pub(super) fn dns_factory<H: CoreInstanceHost>(
    runtime: &Arc<DataPlaneRuntime<H>>,
) -> Arc<dyn VpnDnsSocketFactory> {
    Arc::new(PacketDnsFactory {
        runtime: Arc::downgrade(runtime),
    })
}
