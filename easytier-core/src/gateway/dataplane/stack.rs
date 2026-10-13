//! smoltcp stack generation and its peer-packet bridge.

use std::{
    net::IpAddr,
    sync::{Arc, Weak},
};

use smoltcp::wire::{Ipv4Packet, Ipv6Packet};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinSet,
};

use crate::{
    foundation::task::reap_joinset_background,
    gateway::smoltcp::{BufferSize, Net, NetConfig, channel_device},
    packet::ZCPacket,
    peers::peer_manager::PeerManagerCore,
};

use super::{DataPlaneErrorKind, DataPlaneIoGuard};

pub(super) struct SmoltcpPlane {
    pub(super) ipv4_addr: cidr::Ipv4Inet,
    pub(super) net: Arc<Net>,
    generation: DataPlaneIoGuard,
    _forward_tasks: Arc<std::sync::Mutex<JoinSet<()>>>,
}

impl SmoltcpPlane {
    pub(super) fn new(
        ipv4_addr: cidr::Ipv4Inet,
        ipv6_addr: Option<cidr::Ipv6Inet>,
        peer_manager: Weak<PeerManagerCore>,
        packet_recv: Arc<Mutex<mpsc::Receiver<ZCPacket>>>,
    ) -> Self {
        let mut forward_tasks = JoinSet::new();
        let mut capabilities = smoltcp::phy::DeviceCapabilities::default();
        // Fragment offsets are expressed in eight-byte units.
        capabilities.max_transmission_unit = 1284;
        capabilities.medium = smoltcp::phy::Medium::Ip;
        let (device, stack_sink, mut stack_stream) =
            channel_device::ChannelDevice::new(capabilities);

        forward_tasks.spawn(async move {
            let mut packet_recv = packet_recv.lock().await;
            while let Some(packet) = packet_recv.recv().await {
                tracing::trace!(?packet, "deliver peer packet to smoltcp");
                if let Err(error) = stack_sink.send(Ok(packet.payload().to_vec())).await {
                    tracing::error!(?error, "deliver peer packet to smoltcp failed");
                }
            }
            tracing::debug!("peer-to-smoltcp bridge stopped");
        });

        forward_tasks.spawn(async move {
            while let Some(data) = stack_stream.recv().await {
                let destination = match data.first().map(|v| v >> 4) {
                    Some(4) => Ipv4Packet::new_checked(&data).map(|p| IpAddr::V4(p.dst_addr())),
                    Some(6) => Ipv6Packet::new_checked(&data).map(|p| IpAddr::V6(p.dst_addr())),
                    _ => {
                        tracing::error!("smoltcp emitted an invalid IP packet");
                        continue;
                    }
                };
                let Ok(destination) = destination else {
                    continue;
                };
                let Some(peer_manager) = peer_manager.upgrade() else {
                    tracing::debug!("smoltcp-to-peer bridge lost PeerManager");
                    return;
                };
                if let Err(error) = peer_manager
                    .send_msg_by_ip(ZCPacket::new_with_payload(&data), destination, false)
                    .await
                {
                    tracing::error!(?error, "deliver smoltcp packet to peer failed");
                }
            }
            tracing::debug!("smoltcp-to-peer bridge stopped");
        });

        // A fixed seed replays IP IDs after the last consumer closes and the
        // stack is recreated, potentially reusing a remote shallow-cache route.
        let interface_config = Self::interface_config(rand::random());
        let mut net_config = NetConfig::new(
            interface_config,
            format!("{}/{}", ipv4_addr.address(), ipv4_addr.network_length())
                .parse()
                .expect("validated IPv4 prefix"),
            vec![
                ipv4_addr
                    .address()
                    .to_string()
                    .parse()
                    .expect("validated IPv4 address"),
            ],
            Some(BufferSize {
                tcp_rx_size: 1024 * 128,
                tcp_tx_size: 1024 * 128,
                udp_rx_size: 1024 * 128,
                udp_tx_size: 1024 * 128,
                udp_rx_meta_size: 128,
                udp_tx_meta_size: 128,
            }),
        );
        if let Some(v6) = ipv6_addr {
            net_config = net_config
                .with_additional_ip(v6.to_string().parse().expect("validated IPv6 prefix"));
            net_config.gateway.push(v6.address().into());
        }
        let net = Net::new(device, net_config);

        let forward_tasks = Arc::new(std::sync::Mutex::new(forward_tasks));
        forward_tasks.lock().unwrap().spawn(reap_joinset_background(
            Arc::downgrade(&forward_tasks),
            "SmoltcpPlane",
        ));

        Self {
            ipv4_addr,
            net: Arc::new(net),
            generation: DataPlaneIoGuard::new(),
            _forward_tasks: forward_tasks,
        }
    }

    pub(super) fn interface_config(random_seed: u64) -> smoltcp::iface::Config {
        let mut config = smoltcp::iface::Config::new(smoltcp::wire::HardwareAddress::Ip);
        config.random_seed = random_seed;
        config
    }

    pub(super) fn lease(&self) -> DataPlaneIoGuard {
        self.generation.clone()
    }

    pub(super) fn close(&self, kind: DataPlaneErrorKind) {
        self.generation.close(kind);
    }
}

impl Drop for SmoltcpPlane {
    fn drop(&mut self) {
        self.close(DataPlaneErrorKind::HandleClosed);
    }
}
