use std::sync::Arc;

#[cfg(feature = "wrapped-transport")]
use easytier_core::gateway::proxy::wrapped_transport::WrappedTransportEngines;
#[cfg(feature = "wireguard")]
use easytier_core::gateway::vpn_portal::PortalHost;
#[cfg(test)]
use easytier_core::host::packet::{HostPacket, PacketSink};
#[cfg(feature = "web-client")]
use easytier_core::{
    connectivity::manual::ManualTunnelConnector,
    host::dns::{DnsRecordResolver, DnsResolver},
};
use easytier_core::{
    events::{CoreEvent, CoreEventSink},
    instance::{CoreHostAdapters, CoreInstance, CoreInstanceConfig, PacketEgressHost},
    process_runtime::CoreProcessRuntime,
};

use crate::common::global_ctx::GlobalCtxEvent;
#[cfg(feature = "public-ipv6-provider")]
use crate::instance::public_ipv6_provider::runtime_public_ipv6_provider_platform;
use crate::{
    common::global_ctx::ArcGlobalCtx,
    common::{config::TomlConfig, global_ctx::GlobalCtx},
    host_runtime::native_host_runtime,
    instance::config::{
        compact_runtime_core_host_config, runtime_core_host_config, runtime_peer_credential_storage,
    },
    instance::listeners::RuntimeExternalListenerFactory,
    instance::runtime_host::NativeInstanceRuntimeHost,
};

use super::host::{NativeInstanceHost, native_instance_host};
#[cfg(feature = "kcp")]
use crate::gateway::kcp_proxy::KcpProxyService;
#[cfg(feature = "quic")]
use crate::gateway::quic_proxy::QuicProxyService;
use crate::tunnel::protocol::{runtime_client_protocol_upgrader, runtime_server_protocol_upgrader};
#[cfg(any(feature = "kcp", feature = "quic"))]
use easytier_core::gateway::proxy::wrapped_transport::WrappedTransportEngine;

pub(crate) type NativeCoreInstance = CoreInstance<NativeInstanceHost>;

pub(crate) fn compose_native_core_instance(
    toml_config: TomlConfig,
    process_runtime: Arc<CoreProcessRuntime>,
    compact_runtime: bool,
) -> anyhow::Result<Arc<NativeCoreInstance>> {
    let host_config = if compact_runtime {
        compact_runtime_core_host_config()
    } else {
        runtime_core_host_config()
    };
    let normalized = CoreInstanceConfig::from_toml_with_host(&toml_config, &host_config)?;
    let global_ctx = Arc::new(GlobalCtx::new_with_runtime_config(
        toml_config.clone(),
        &normalized,
        &host_config,
    ));
    let runtime_host = NativeInstanceRuntimeHost::new(global_ctx.clone());
    let mut adapters = runtime_core_host_adapters_with_packet_egress_and_config(
        global_ctx.clone(),
        process_runtime,
        runtime_host.clone(),
        host_config,
    );
    adapters.instance_runtime = runtime_host;
    NativeCoreInstance::from_toml(toml_config, adapters)
}

impl CoreEventSink for GlobalCtx {
    fn emit(&self, event: CoreEvent) {
        let event = match event {
            CoreEvent::PeerAdded(peer_id) => GlobalCtxEvent::PeerAdded(peer_id),
            CoreEvent::PeerRemoved(peer_id) => GlobalCtxEvent::PeerRemoved(peer_id),
            CoreEvent::PeerConnAdded(info) => GlobalCtxEvent::PeerConnAdded(info.into()),
            CoreEvent::PeerConnRemoved(info) => GlobalCtxEvent::PeerConnRemoved(info.into()),
            CoreEvent::CredentialChanged => GlobalCtxEvent::CredentialChanged,
            CoreEvent::ManualConnecting { url } => GlobalCtxEvent::Connecting(url),
            CoreEvent::ManualConnectError {
                url,
                ip_version,
                error,
            } => GlobalCtxEvent::ConnectError(url.to_string(), format!("{ip_version:?}"), error),
            CoreEvent::ListenerPlanFailed { url, error } => {
                GlobalCtxEvent::ListenerAddFailed(url, error)
            }
            CoreEvent::ListenerAdded { url, .. } => GlobalCtxEvent::ListenerAdded(url),
            CoreEvent::ListenerRemoved { .. } | CoreEvent::ListenerSocketAccepted { .. } => return,
            CoreEvent::ListenerAddFailed {
                url,
                error,
                will_retry,
                ..
            } => {
                let message = if will_retry {
                    format!("error: {error}, retry listen later...")
                } else {
                    format!("error: {error}")
                };
                GlobalCtxEvent::ListenerAddFailed(url, message)
            }
            CoreEvent::ListenerAcceptFailed { url, error } => GlobalCtxEvent::ListenerAcceptFailed(
                url,
                format!("error: {error}, retry listen later..."),
            ),
            CoreEvent::ListenerAcceptedSocketHandleFailed { url, error } => {
                tracing::error!(%url, %error, "accepted socket handler failed");
                return;
            }
            CoreEvent::TunnelAccepted {
                local_url,
                remote_url,
            } => GlobalCtxEvent::ConnectionAccepted(local_url, remote_url),
            CoreEvent::TunnelAdmissionFailed {
                local_url,
                remote_url,
                error,
            } => GlobalCtxEvent::ConnectionError(local_url, remote_url, error),
            CoreEvent::UdpPortMappingEstablished {
                local_listener,
                mapped_listener,
                backend,
            } => GlobalCtxEvent::ListenerPortMappingEstablished {
                local_listener,
                mapped_listener,
                backend,
            },
            CoreEvent::ProxyCidrsUpdated { added, removed } => {
                GlobalCtxEvent::ProxyCidrsUpdated(added, removed)
            }
            CoreEvent::PublicIpv6LeaseChanged { old, new } => {
                GlobalCtxEvent::PublicIpv6Changed(old, new)
            }
            CoreEvent::PublicIpv6RoutesChanged { added, removed } => {
                GlobalCtxEvent::PublicIpv6RoutesUpdated(added, removed)
            }
            CoreEvent::VpnPortalStarted(portal) => GlobalCtxEvent::VpnPortalStarted(portal),
            CoreEvent::VpnPortalClientConnected { portal, client } => {
                GlobalCtxEvent::VpnPortalClientConnected(portal, client)
            }
            CoreEvent::VpnPortalClientDisconnected { portal, client } => {
                GlobalCtxEvent::VpnPortalClientDisconnected(portal, client)
            }
            CoreEvent::GatewayPortForwardAdded(config) => {
                GlobalCtxEvent::PortForwardAdded(config.into())
            }
        };
        self.issue_event(event);
    }
}

#[cfg(feature = "wrapped-transport")]
fn runtime_wrapped_transport_engines(
    config: &easytier_core::instance::CoreInstanceHostConfig,
    enable_bbr: bool,
) -> WrappedTransportEngines {
    let _ = enable_bbr;
    #[cfg(feature = "kcp")]
    let kcp = config
        .kcp_enabled
        .then(|| Arc::new(KcpProxyService::new()) as Arc<dyn WrappedTransportEngine>);
    #[cfg(not(feature = "kcp"))]
    let kcp = None;
    #[cfg(feature = "quic")]
    let quic = config
        .quic_enabled
        .then(|| Arc::new(QuicProxyService::new(enable_bbr)) as Arc<dyn WrappedTransportEngine>);
    #[cfg(not(feature = "quic"))]
    let quic = None;

    WrappedTransportEngines { kcp, quic }
}

#[cfg(test)]
pub(crate) fn runtime_core_host_adapters(
    global_ctx: ArcGlobalCtx,
    process_runtime: Arc<CoreProcessRuntime>,
    packet_sink: Arc<dyn PacketSink>,
) -> CoreHostAdapters<NativeInstanceHost> {
    let host = native_instance_host(global_ctx.clone());
    let runtime_dns = native_host_runtime();
    let adapters = CoreHostAdapters::new(host, runtime_dns, packet_sink, process_runtime);
    configure_runtime_core_host_adapters(global_ctx, adapters, runtime_core_host_config())
}

#[cfg(test)]
pub(crate) fn runtime_core_host_adapters_with_packet_egress(
    global_ctx: ArcGlobalCtx,
    process_runtime: Arc<CoreProcessRuntime>,
    packet_egress: Arc<dyn PacketEgressHost>,
) -> CoreHostAdapters<NativeInstanceHost> {
    let host = native_instance_host(global_ctx.clone());
    let runtime_dns = native_host_runtime();
    let adapters =
        CoreHostAdapters::new_with_packet_egress(host, runtime_dns, packet_egress, process_runtime);
    configure_runtime_core_host_adapters(global_ctx, adapters, runtime_core_host_config())
}

fn runtime_core_host_adapters_with_packet_egress_and_config(
    global_ctx: ArcGlobalCtx,
    process_runtime: Arc<CoreProcessRuntime>,
    packet_egress: Arc<dyn PacketEgressHost>,
    host_config: easytier_core::instance::CoreInstanceHostConfig,
) -> CoreHostAdapters<NativeInstanceHost> {
    let host = native_instance_host(global_ctx.clone());
    let runtime_dns = native_host_runtime();
    let adapters =
        CoreHostAdapters::new_with_packet_egress(host, runtime_dns, packet_egress, process_runtime);
    configure_runtime_core_host_adapters(global_ctx, adapters, host_config)
}

fn configure_runtime_core_host_adapters(
    global_ctx: ArcGlobalCtx,
    mut adapters: CoreHostAdapters<NativeInstanceHost>,
    host_config: easytier_core::instance::CoreInstanceHostConfig,
) -> CoreHostAdapters<NativeInstanceHost> {
    #[cfg(test)]
    adapters.replace_stun_provider(Arc::new(crate::common::stun::MockStunInfoCollector {
        udp_nat_type: crate::proto::common::NatType::Unknown,
    }));
    adapters.config = host_config.clone();
    adapters.credential_storage = (!host_config.ignore_unsupported_config)
        .then(|| runtime_peer_credential_storage(&global_ctx))
        .flatten();
    adapters.events = global_ctx.clone();
    #[cfg(feature = "wrapped-transport")]
    {
        adapters.wrapped_transports =
            runtime_wrapped_transport_engines(&host_config, global_ctx.get_flags().enable_bbr);
    }
    adapters.protocol = Some(runtime_client_protocol_upgrader(global_ctx.clone()));
    adapters.external_listener_factory = Some(Arc::new(RuntimeExternalListenerFactory));
    adapters.server_protocol = Some(runtime_server_protocol_upgrader(global_ctx.clone()));
    adapters.server_protocol_for_connected =
        Some(runtime_server_protocol_upgrader(global_ctx.clone()));
    #[cfg(feature = "upnp")]
    if host_config.upnp_enabled {
        adapters.udp_hole_punch_platform = Some(
            crate::instance::udp_hole_punch::runtime_udp_hole_punch_platform(
                global_ctx.net_ns.clone(),
            ),
        );
    }
    #[cfg(feature = "icmp-proxy")]
    {
        adapters.icmp_proxy_host = Some(Arc::new(crate::gateway::icmp_proxy::RuntimeIcmpProxyHost));
    }
    #[cfg(feature = "proxy-cidr-monitor")]
    {
        adapters.proxy_cidr_monitor_enabled = true;
    }
    #[cfg(feature = "public-ipv6-provider")]
    if host_config.public_ipv6_provider_supported {
        adapters.public_ipv6_host = Some(global_ctx.clone());
        adapters.public_ipv6_provider = Some(runtime_public_ipv6_provider_platform(&global_ctx));
    }
    #[cfg(feature = "wireguard")]
    if host_config.vpn_portal_enabled {
        use crate::common::config::ConfigLoader as _;

        if let Some(config) = global_ctx.config.get_vpn_portal_config() {
            adapters.vpn_portal = Some(crate::vpn_portal::wireguard::WireGuardPortalHost::new(
                global_ctx.clone(),
                config,
            ) as Arc<dyn PortalHost>);
        }
    }
    adapters
}

#[cfg(feature = "web-client")]
pub(crate) fn runtime_one_shot_manual_connector(
    global_ctx: ArcGlobalCtx,
    config: &TomlConfig,
    process_runtime: Arc<CoreProcessRuntime>,
) -> anyhow::Result<ManualTunnelConnector<NativeInstanceHost>> {
    let normalized = CoreInstanceConfig::from_toml_with_host(config, &runtime_core_host_config())?;
    let host = native_instance_host(global_ctx.clone());
    let runtime_dns = native_host_runtime();
    let dns: Arc<dyn DnsResolver> = runtime_dns.clone();
    let dns_records: Arc<dyn DnsRecordResolver> = runtime_dns;
    Ok(process_runtime.manual_connector(
        host,
        dns,
        dns_records,
        runtime_client_protocol_upgrader(global_ctx.clone()),
        normalized.connectivity.endpoint_discovery,
        normalized.connectivity.manual,
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    #[cfg(feature = "kcp")]
    use easytier_core::gateway::proxy::wrapped_transport::{
        WrappedTransportConnect, WrappedTransportEngine,
    };
    use easytier_core::listener::plan::ListenerRuntimeConfig;
    use smoltcp::wire::{IpAddress, IpProtocol, Ipv4Packet, UdpPacket};
    #[cfg(feature = "kcp")]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[cfg(feature = "kcp")]
    use crate::gateway::kcp_proxy::KcpProxyService;
    use crate::{
        common::{config::NetworkIdentity, global_ctx::tests::get_mock_global_ctx_with_network},
        instance::config::test_core_instance_config,
    };

    use super::*;

    fn create_host_packet_channel() -> (
        tokio::sync::mpsc::Sender<Vec<u8>>,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        tokio::sync::mpsc::channel(16)
    }

    #[cfg(feature = "kcp")]
    fn build_native_kcp_test_instance(
        global_ctx: ArcGlobalCtx,
        packet_sink: tokio::sync::mpsc::Sender<Vec<u8>>,
        listeners: Option<ListenerRuntimeConfig>,
    ) -> anyhow::Result<(Arc<NativeCoreInstance>, Arc<KcpProxyService>)> {
        let mut adapters = runtime_core_host_adapters(
            global_ctx.clone(),
            CoreProcessRuntime::new(),
            Arc::new(packet_sink),
        );
        adapters.proxy_cidr_monitor_enabled = false;
        let service = Arc::new(KcpProxyService::new());
        adapters.wrapped_transports = WrappedTransportEngines {
            kcp: Some(service.clone()),
            quic: None,
        };

        let mut config = test_core_instance_config(&global_ctx);
        config.connectivity.listeners = listeners;
        config.connectivity.startup_plan.gateway = false;
        config.connectivity.stun.udp_servers.clear();
        config.connectivity.stun.tcp_servers.clear();
        config.connectivity.stun.udp_v6_servers.clear();
        config.connectivity.manual = Default::default();
        config.connectivity.direct.testing = true;

        let instance = NativeCoreInstance::new(config, adapters)?;
        Ok((instance, service))
    }

    #[cfg(feature = "kcp")]
    #[tokio::test]
    async fn native_kcp_engine_round_trips_through_portable_cores() {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            const REQUEST: &[u8] = b"native-kcp-request";
            const REPLY: &[u8] = b"native-kcp-reply";

            let global_a = get_mock_global_ctx_with_network(Some(NetworkIdentity::new(
                "native-kcp-round-trip".to_owned(),
                "shared-secret".to_owned(),
            )));
            let global_b = get_mock_global_ctx_with_network(Some(NetworkIdentity::new(
                "native-kcp-round-trip".to_owned(),
                "shared-secret".to_owned(),
            )));
            global_a.set_ipv4(Some("10.250.0.1/24".parse().unwrap()));
            global_b.set_ipv4(Some("10.250.0.2/24".parse().unwrap()));

            let mut flags_a = global_a.get_flags();
            flags_a.enable_kcp_proxy = true;
            flags_a.disable_kcp_input = true;
            flags_a.disable_tcp_hole_punching = true;
            flags_a.disable_udp_hole_punching = true;
            flags_a.disable_sym_hole_punching = true;
            flags_a.disable_upnp = true;
            global_a.set_flags(flags_a);

            let mut flags_b = global_b.get_flags();
            flags_b.enable_kcp_proxy = false;
            flags_b.disable_kcp_input = false;
            flags_b.disable_tcp_hole_punching = true;
            flags_b.disable_udp_hole_punching = true;
            flags_b.disable_sym_hole_punching = true;
            flags_b.disable_upnp = true;
            global_b.set_flags(flags_b);

            let (packet_sink_a, _packet_receiver_a) = create_host_packet_channel();
            let (packet_sink_b, _packet_receiver_b) = create_host_packet_channel();
            let (instance_a, kcp_a) = build_native_kcp_test_instance(
                global_a.clone(),
                packet_sink_a,
                Some(ListenerRuntimeConfig::new(
                    vec!["tcp://127.0.0.1:0".parse().unwrap()],
                    false,
                    test_core_instance_config(&global_a)
                        .connectivity
                        .direct
                        .tcp_bind
                        .context,
                )),
            )
            .unwrap();
            let (instance_b, _kcp_b) =
                build_native_kcp_test_instance(global_b, packet_sink_b, None).unwrap();

            let (start_a, start_b) = tokio::join!(instance_a.start(), instance_b.start());
            start_a.unwrap();
            start_b.unwrap();

            let listener = instance_a.running_listeners().pop().unwrap();
            instance_b.add_connector(listener).unwrap();
            let peer_a_id = instance_a.peer_id();
            let peer_b_id = instance_b.peer_id();
            loop {
                let a_peers = instance_a.connected_peers().await;
                let b_peers = instance_b.connected_peers().await;
                if a_peers.contains(&peer_b_id) && b_peers.contains(&peer_a_id) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }

            let echo_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let echo_addr = echo_listener.local_addr().unwrap();
            let responder = tokio::spawn(async move {
                let (mut socket, _) = echo_listener.accept().await.unwrap();
                let mut request = [0; REQUEST.len()];
                socket.read_exact(&mut request).await.unwrap();
                assert_eq!(&request, REQUEST);
                socket.write_all(REPLY).await.unwrap();
            });

            let mut stream = kcp_a
                .connect_source(WrappedTransportConnect {
                    my_peer_id: peer_a_id,
                    dst_peer_id: peer_b_id,
                    src: "10.250.0.1:40000".parse().unwrap(),
                    dst: echo_addr,
                })
                .await
                .unwrap();
            stream.write_all(REQUEST).await.unwrap();
            let mut reply = [0; REPLY.len()];
            stream.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, REPLY);
            responder.await.unwrap();

            instance_b.stop().await;
            instance_a.stop().await;
        })
        .await
        .expect("native KCP round trip timed out");
    }

    #[tokio::test]
    async fn portable_core_instances_connect_through_core_tcp_listener() {
        let global_a = get_mock_global_ctx_with_network(Some(NetworkIdentity::new(
            "portable-connect-listen".to_owned(),
            "shared-secret".to_owned(),
        )));
        let global_b = get_mock_global_ctx_with_network(Some(NetworkIdentity::new(
            "portable-connect-listen".to_owned(),
            "shared-secret".to_owned(),
        )));
        global_a.set_ipv4(Some("10.250.0.1/24".parse().unwrap()));
        global_b.set_ipv4(Some("10.250.0.2/24".parse().unwrap()));
        let (packet_sink_a, _packet_receiver_a) = create_host_packet_channel();
        let (packet_sink_b, mut packet_receiver_b) = create_host_packet_channel();
        let mut config_a = test_core_instance_config(&global_a);
        config_a.connectivity.initial_peers.clear();
        config_a.connectivity.listeners = Some(ListenerRuntimeConfig::new(
            vec!["tcp://127.0.0.1:0".parse().unwrap()],
            false,
            config_a.connectivity.direct.tcp_bind.context.clone(),
        ));
        config_a.connectivity.runtime = Default::default();
        config_a.connectivity.stun.udp_servers.clear();
        config_a.connectivity.stun.tcp_servers.clear();
        config_a.connectivity.stun.udp_v6_servers.clear();
        config_a.connectivity.manual = Default::default();
        config_a.connectivity.direct.testing = true;
        let instance_a = NativeCoreInstance::new(
            config_a,
            runtime_core_host_adapters(
                global_a.clone(),
                CoreProcessRuntime::new(),
                Arc::new(packet_sink_a),
            ),
        )
        .unwrap();

        let mut config_b = test_core_instance_config(&global_b);
        config_b.connectivity.initial_peers.clear();
        config_b.connectivity.listeners = None;
        config_b.connectivity.runtime = Default::default();
        config_b.connectivity.stun.udp_servers.clear();
        config_b.connectivity.stun.tcp_servers.clear();
        config_b.connectivity.stun.udp_v6_servers.clear();
        config_b.connectivity.manual = Default::default();
        config_b.connectivity.direct.testing = true;
        let instance_b = NativeCoreInstance::new(
            config_b,
            runtime_core_host_adapters(
                global_b.clone(),
                CoreProcessRuntime::new(),
                Arc::new(packet_sink_b),
            ),
        )
        .unwrap();

        let (start_a, start_b) = tokio::join!(instance_a.start(), instance_b.start());
        start_a.unwrap();
        start_b.unwrap();
        let listener = instance_a.running_listeners().pop().unwrap();
        instance_b.add_connector(listener).unwrap();

        let peer_a_id = instance_a.peer_id();
        let peer_b_id = instance_b.peer_id();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let a_peers = instance_a.connected_peers().await;
                let b_peers = instance_b.connected_peers().await;
                if a_peers.contains(&peer_b_id) && b_peers.contains(&peer_a_id) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("portable core instances did not connect through the core listener");

        let source_ip = "10.250.0.1".parse().unwrap();
        let destination_ip = "10.250.0.2".parse().unwrap();
        let mut ip_packet = vec![0u8; 28];
        {
            let mut ipv4 = Ipv4Packet::new_unchecked(&mut ip_packet);
            ipv4.set_version(4);
            ipv4.set_header_len(20);
            ipv4.set_total_len(28);
            ipv4.set_hop_limit(64);
            ipv4.set_next_header(IpProtocol::Udp);
            ipv4.set_src_addr(source_ip);
            ipv4.set_dst_addr(destination_ip);
        }
        {
            let mut udp = UdpPacket::new_unchecked(&mut ip_packet[20..]);
            udp.set_src_port(10000);
            udp.set_dst_port(10001);
            udp.set_len(8);
            udp.fill_checksum(
                &IpAddress::Ipv4(source_ip),
                &IpAddress::Ipv4(destination_ip),
            );
        }
        Ipv4Packet::new_unchecked(&mut ip_packet).fill_checksum();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                instance_a
                    .packet_plane()
                    .send_ip_packet(HostPacket::copy_from_payload(&ip_packet))
                    .await
                    .unwrap();
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    packet_receiver_b.recv(),
                )
                .await
                {
                    Ok(Some(packet)) => break packet,
                    Ok(None) => panic!("portable host packet sink closed"),
                    Err(_) => {}
                }
            }
        })
        .await
        .expect("portable host packet sink did not receive the IP packet");
        assert_eq!(received, ip_packet);

        instance_b.stop().await;
        instance_a.stop().await;
    }

    #[cfg(feature = "websocket")]
    #[tokio::test]
    async fn portable_core_instances_connect_through_core_wss_listener_with_prefer_policy() {
        let global_a = get_mock_global_ctx_with_network(Some(NetworkIdentity::new(
            "portable-connect-wss".to_owned(),
            "shared-secret".to_owned(),
        )));
        let global_b = get_mock_global_ctx_with_network(Some(NetworkIdentity::new(
            "portable-connect-wss".to_owned(),
            "shared-secret".to_owned(),
        )));
        global_a.set_ipv4(Some("10.251.0.1/24".parse().unwrap()));
        global_b.set_ipv4(Some("10.251.0.2/24".parse().unwrap()));
        for global in [&global_a, &global_b] {
            let mut flags = global.get_flags();
            flags.prefer_wss_http3_for_p2p = true;
            flags.disable_wss_http3_for_p2p = false;
            flags.only_use_wss_http3_for_hole_punching = false;
            flags.disable_tcp_hole_punching = true;
            flags.disable_udp_hole_punching = true;
            flags.disable_upnp = true;
            global.set_flags(flags);
        }

        let (packet_sink_a, _packet_receiver_a) = create_host_packet_channel();
        let (packet_sink_b, _packet_receiver_b) = create_host_packet_channel();
        let mut config_a = test_core_instance_config(&global_a);
        config_a.connectivity.initial_peers.clear();
        config_a.connectivity.listeners = Some(ListenerRuntimeConfig::new(
            vec!["wss://127.0.0.1:0".parse().unwrap()],
            false,
            config_a.connectivity.direct.tcp_bind.context.clone(),
        ));
        config_a.connectivity.runtime = Default::default();
        config_a.connectivity.stun.udp_servers.clear();
        config_a.connectivity.stun.tcp_servers.clear();
        config_a.connectivity.stun.udp_v6_servers.clear();
        config_a.connectivity.manual = Default::default();
        config_a.connectivity.direct.testing = true;
        let instance_a = NativeCoreInstance::new(
            config_a,
            runtime_core_host_adapters(
                global_a.clone(),
                CoreProcessRuntime::new(),
                Arc::new(packet_sink_a),
            ),
        )
        .unwrap();

        let mut config_b = test_core_instance_config(&global_b);
        config_b.connectivity.initial_peers.clear();
        config_b.connectivity.listeners = None;
        config_b.connectivity.runtime = Default::default();
        config_b.connectivity.stun.udp_servers.clear();
        config_b.connectivity.stun.tcp_servers.clear();
        config_b.connectivity.stun.udp_v6_servers.clear();
        config_b.connectivity.manual = Default::default();
        config_b.connectivity.direct.testing = true;
        let instance_b = NativeCoreInstance::new(
            config_b,
            runtime_core_host_adapters(
                global_b.clone(),
                CoreProcessRuntime::new(),
                Arc::new(packet_sink_b),
            ),
        )
        .unwrap();

        let (start_a, start_b) = tokio::join!(instance_a.start(), instance_b.start());
        start_a.unwrap();
        start_b.unwrap();
        let listener = instance_a
            .running_listeners()
            .into_iter()
            .find(|url| url.scheme() == "wss")
            .expect("wss listener should be running");
        instance_b.add_connector(listener).unwrap();

        let peer_a_id = instance_a.peer_id();
        let peer_b_id = instance_b.peer_id();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let a_peers = instance_a.connected_peers().await;
                let b_peers = instance_b.connected_peers().await;
                if a_peers.contains(&peer_b_id) && b_peers.contains(&peer_a_id) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("portable core instances did not connect through the core WSS listener");

        instance_b.stop().await;
        instance_a.stop().await;
    }

    #[cfg(feature = "quic")]
    mod native_http3 {
        use std::{
            net::{Ipv4Addr, SocketAddr},
            sync::Mutex,
            time::{Duration, Instant},
        };

        use crate::proto::common::TunnelInfo;
        use easytier_core::{
            config::PeerId,
            packet::{UDP_TUNNEL_HEADER_SIZE, UDPTunnelHeader},
            socket::udp::parse_quic_initial_dcid,
        };
        use zerocopy::FromBytes;

        use super::*;

        fn build_instance(
            network: &str,
            overlay_ip: &str,
            listeners: Vec<url::Url>,
            automatic_p2p: bool,
        ) -> (
            Arc<NativeCoreInstance>,
            tokio::sync::mpsc::Receiver<Vec<u8>>,
        ) {
            let global = get_mock_global_ctx_with_network(Some(NetworkIdentity::new(
                network.to_owned(),
                "shared-secret".to_owned(),
            )));
            global.set_ipv4(Some(overlay_ip.parse().unwrap()));
            let mut flags = global.get_flags();
            flags.enable_ipv6 = true;
            flags.default_protocol = "udp".to_owned();
            flags.disable_p2p = !automatic_p2p;
            flags.lazy_p2p = false;
            flags.prefer_wss_http3_for_p2p = automatic_p2p;
            flags.disable_wss_http3_for_p2p = false;
            flags.only_use_wss_http3_for_hole_punching = false;
            flags.close_redundant_conns_when_disguised = true;
            flags.disable_tcp_hole_punching = true;
            flags.disable_udp_hole_punching = true;
            flags.disable_sym_hole_punching = true;
            flags.disable_upnp = true;
            global.set_flags(flags);

            let (sink, packets) = create_host_packet_channel();
            let mut config = test_core_instance_config(&global);
            tracing::debug!(?config.peer.snapshot.flags, "native HTTP3 test instance flags");
            config.connectivity.initial_peers.clear();
            config.connectivity.listeners = (!listeners.is_empty()).then(|| {
                ListenerRuntimeConfig::new(
                    listeners,
                    false,
                    config.connectivity.direct.udp_bind.context.clone(),
                )
                .with_udp_http3(true)
            });
            config.connectivity.startup_plan.gateway = false;
            config.connectivity.startup_plan.packet_proxy = false;
            config.connectivity.stun.udp_servers.clear();
            config.connectivity.stun.tcp_servers.clear();
            config.connectivity.stun.udp_v6_servers.clear();
            config.connectivity.manual = Default::default();
            config.connectivity.direct.testing = true;
            let mut adapters =
                runtime_core_host_adapters(global, CoreProcessRuntime::new(), Arc::new(sink));
            #[cfg(feature = "proxy-cidr-monitor")]
            {
                adapters.proxy_cidr_monitor_enabled = false;
            }
            adapters.udp_hole_punch_platform = None;
            (NativeCoreInstance::new(config, adapters).unwrap(), packets)
        }

        async fn wait_for_connection(
            instance: &NativeCoreInstance,
            peer_id: PeerId,
            scheme: &str,
            selected: bool,
        ) -> TunnelInfo {
            let result = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    for peer in instance.peer_snapshots().await {
                        if peer.peer_id != peer_id {
                            continue;
                        }
                        let default_conn = peer.default_conn_id.map(|id| id.to_string());
                        for conn in peer.conns {
                            if conn.is_closed
                                || (selected
                                    && default_conn.as_deref() != Some(conn.conn_id.as_str()))
                            {
                                continue;
                            }
                            if let Some(tunnel) = conn.tunnel
                                && tunnel.tunnel_type.rsplit('-').next() == Some(scheme)
                            {
                                return tunnel;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            match result {
                Ok(tunnel) => tunnel,
                Err(_) => panic!(
                    "peer {peer_id} did not acquire a {scheme} connection (selected={selected}); \
                     local_peer={}; listeners={:?}; connectors={:?}; peers={:#?}; routes={:#?}; \
                     latest_error={:?}",
                    instance.peer_id(),
                    instance.running_listeners(),
                    instance.list_connectors(),
                    instance.peer_snapshots().await,
                    instance.route_snapshots().await,
                    instance.latest_error(),
                ),
            }
        }

        fn ip_packet(source: Ipv4Addr, destination: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
            let mut packet = vec![0; 28 + payload.len()];
            let packet_len = packet.len() as u16;
            {
                let mut ipv4 = Ipv4Packet::new_unchecked(&mut packet);
                ipv4.set_version(4);
                ipv4.set_header_len(20);
                ipv4.set_total_len(packet_len);
                ipv4.set_hop_limit(64);
                ipv4.set_next_header(IpProtocol::Udp);
                ipv4.set_src_addr(source);
                ipv4.set_dst_addr(destination);
            }
            {
                let mut udp = UdpPacket::new_unchecked(&mut packet[20..]);
                udp.set_src_port(10000);
                udp.set_dst_port(10001);
                udp.set_len(8 + payload.len() as u16);
                udp.payload_mut().copy_from_slice(payload);
                udp.fill_checksum(&IpAddress::Ipv4(source), &IpAddress::Ipv4(destination));
            }
            Ipv4Packet::new_unchecked(&mut packet).fill_checksum();
            packet
        }

        async fn deliver_packet(
            sender: &NativeCoreInstance,
            receiver: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
            packet: &[u8],
        ) {
            let received = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    sender
                        .packet_plane()
                        .send_ip_packet(HostPacket::copy_from_payload(packet))
                        .await
                        .unwrap();
                    match tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await {
                        Ok(Some(received)) if received == packet => return received,
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("native host packet sink closed"),
                        Err(_) => {}
                    }
                }
            })
            .await
            .expect("native host packet round trip timed out");
            assert_eq!(received, packet);
        }

        const BURST_MARKER: &[u8] = b"native-http3-encrypted-burst";

        #[derive(Debug, Default)]
        struct WireObservations {
            datagrams: usize,
            bytes: usize,
            initial_datagrams: usize,
            nonstandard_initial_versions: usize,
            easytier_envelopes: usize,
            plaintext_markers: usize,
            first_client_datagram: Option<Vec<u8>>,
        }

        impl WireObservations {
            fn observe(&mut self, datagram: &[u8], from_client: bool) {
                self.datagrams += 1;
                self.bytes += datagram.len();
                if from_client && self.first_client_datagram.is_none() {
                    self.first_client_datagram = Some(datagram.to_vec());
                }
                if parse_quic_initial_dcid(datagram).is_some() {
                    self.initial_datagrams += 1;
                    if datagram[1..5] != 1u32.to_be_bytes() {
                        self.nonstandard_initial_versions += 1;
                    }
                }
                if let Some(header) = UDPTunnelHeader::ref_from_prefix(datagram)
                    && header.padding == 0
                    && (1..=7).contains(&header.msg_type)
                    && usize::from(header.len.get()) + UDP_TUNNEL_HEADER_SIZE == datagram.len()
                {
                    self.easytier_envelopes += 1;
                }
                self.plaintext_markers += usize::from(
                    datagram
                        .windows(BURST_MARKER.len())
                        .any(|window| window == BURST_MARKER),
                );
            }
        }

        async fn observe_udp_wire(
            bind: &str,
            server_addr: SocketAddr,
        ) -> (
            SocketAddr,
            Arc<Mutex<WireObservations>>,
            tokio_util::task::AbortOnDropHandle<()>,
        ) {
            let socket = tokio::net::UdpSocket::bind(bind).await.unwrap();
            let address = socket.local_addr().unwrap();
            let observations = Arc::new(Mutex::new(WireObservations::default()));
            let observed = observations.clone();
            let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                let mut buffer = vec![0; 65536];
                let mut client_addr = None;
                loop {
                    let (length, source) = socket.recv_from(&mut buffer).await.unwrap();
                    let from_client = source != server_addr;
                    let target = if from_client {
                        client_addr = Some(source);
                        server_addr
                    } else {
                        client_addr.expect("HTTP3 client must send the first datagram")
                    };
                    observed
                        .lock()
                        .unwrap()
                        .observe(&buffer[..length], from_client);
                    socket.send_to(&buffer[..length], target).await.unwrap();
                }
            }));
            (address, observations, task)
        }

        async fn transfer_burst(
            sender: &NativeCoreInstance,
            receiver: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
            source: Ipv4Addr,
            destination: Ipv4Addr,
            observations: &Mutex<WireObservations>,
        ) -> usize {
            let packets = (0u32..256)
                .map(|sequence| {
                    let mut payload = vec![0x5a; 1024];
                    payload[..BURST_MARKER.len()].copy_from_slice(BURST_MARKER);
                    payload[BURST_MARKER.len()..BURST_MARKER.len() + 4]
                        .copy_from_slice(&sequence.to_be_bytes());
                    ip_packet(source, destination, &payload)
                })
                .collect::<Vec<_>>();
            let mut sent_count = 0;
            let mut received_count = 0;
            let mut ignored_count = 0;
            let mut seen = [false; 256];
            // Keep this transport workload below core's 128-packet NIC queue.
            let credits = tokio::sync::Semaphore::new(64);
            let send = async {
                for packet in &packets {
                    let credit = credits.acquire().await.unwrap();
                    sender
                        .packet_plane()
                        .send_ip_packet(HostPacket::copy_from_payload(packet))
                        .await
                        .unwrap();
                    credit.forget();
                    sent_count += 1;
                }
            };
            let receive = async {
                // The packet plane carries IP datagrams, whose processing may
                // reorder them. Require every exact packet once, without adding
                // an ordering guarantee to the underlying UDP flow.
                let mut count = 0;
                while count < packets.len() {
                    let received = receiver.recv().await.expect("native packet sink closed");
                    received_count += 1;
                    if !received
                        .windows(BURST_MARKER.len())
                        .any(|window| window == BURST_MARKER)
                    {
                        ignored_count += 1;
                        continue;
                    }
                    let sequence_offset = 28 + BURST_MARKER.len();
                    let sequence = u32::from_be_bytes(
                        received
                            .get(sequence_offset..sequence_offset + 4)
                            .expect("truncated burst packet")
                            .try_into()
                            .unwrap(),
                    ) as usize;
                    assert!(
                        sequence < packets.len(),
                        "invalid burst sequence {sequence}"
                    );
                    assert_eq!(
                        received, packets[sequence],
                        "burst packet {sequence} was corrupted"
                    );
                    assert!(!seen[sequence], "duplicate burst packet {sequence}");
                    seen[sequence] = true;
                    count += 1;
                    credits.add_permits(1);
                }
            };
            let result = tokio::time::timeout(Duration::from_secs(15), async {
                tokio::join!(send, receive);
            })
            .await;
            if result.is_err() {
                let missing = seen
                    .iter()
                    .enumerate()
                    .filter_map(|(sequence, seen)| (!seen).then_some(sequence))
                    .collect::<Vec<_>>();
                let wire = format!("{:?}", observations.lock().unwrap());
                panic!(
                    "HTTP3 burst {source} -> {destination} timed out: sent={sent_count}; \
                     received={received_count}; ignored={ignored_count}; missing={missing:?}; \
                     wire={wire}; peers={:#?}; routes={:#?}",
                    sender.peer_snapshots().await,
                    sender.route_snapshots().await,
                );
            }
            packets.iter().map(Vec::len).sum()
        }

        #[rstest::rstest]
        #[tokio::test(flavor = "multi_thread")]
        async fn native_core_http3_companion_preserves_public_wire_and_burst_delivery(
            #[values("127.0.0.1:0", "[::1]:0")] bind: &str,
        ) {
            tokio::time::timeout(Duration::from_secs(90), async {
                for listener_scheme in ["http3", "udp"] {
                    let network = "native-http3-wire-parity";
                    let (server, mut server_packets) = build_instance(
                        network,
                        "10.254.0.1/24",
                        vec![format!("{listener_scheme}://{bind}").parse().unwrap()],
                        false,
                    );
                    let (client, mut client_packets) =
                        build_instance(network, "10.254.0.2/24", Vec::new(), false);
                    let (start_server, start_client) = tokio::join!(server.start(), client.start());
                    start_server.unwrap();
                    start_client.unwrap();
                    let listener = server
                        .running_listeners()
                        .into_iter()
                        .find(|url| url.scheme() == listener_scheme)
                        .unwrap();
                    let server_addr = listener.socket_addrs(|| None).unwrap()[0];
                    let (relay_addr, observations, relay) =
                        observe_udp_wire(bind, server_addr).await;
                    client
                        .add_connector(
                            format!("http3://{relay_addr}?sni=www.example.com")
                                .parse()
                                .unwrap(),
                        )
                        .unwrap();
                    let client_tunnel =
                        wait_for_connection(&client, server.peer_id(), "http3", true).await;
                    let requested_url: url::Url = client_tunnel.remote_addr.unwrap().into();
                    assert_eq!(
                        requested_url
                            .query_pairs()
                            .find(|(key, _)| key == "sni")
                            .map(|(_, value)| value.into_owned())
                            .as_deref(),
                        Some("www.example.com")
                    );
                    wait_for_connection(&server, client.peer_id(), "http3", true).await;
                    let warmup = ip_packet(
                        "10.254.0.2".parse().unwrap(),
                        "10.254.0.1".parse().unwrap(),
                        b"HTTP3 wire test warmup",
                    );
                    deliver_packet(&client, &mut server_packets, &warmup).await;
                    let reply_warmup = ip_packet(
                        "10.254.0.1".parse().unwrap(),
                        "10.254.0.2".parse().unwrap(),
                        b"HTTP3 wire test reply warmup",
                    );
                    deliver_packet(&server, &mut client_packets, &reply_warmup).await;

                    let started = Instant::now();
                    let sent = transfer_burst(
                        &client,
                        &mut server_packets,
                        "10.254.0.2".parse().unwrap(),
                        "10.254.0.1".parse().unwrap(),
                        &observations,
                    )
                    .await;
                    let replied = transfer_burst(
                        &server,
                        &mut client_packets,
                        "10.254.0.1".parse().unwrap(),
                        "10.254.0.2".parse().unwrap(),
                        &observations,
                    )
                    .await;
                    let elapsed = started.elapsed();
                    assert_eq!(sent, 256 * (1024 + 28));
                    assert_eq!(replied, sent);
                    let observed = observations.lock().unwrap();
                    let first = observed.first_client_datagram.as_ref().unwrap();
                    assert!(
                        parse_quic_initial_dcid(first).is_some(),
                        "public HTTP3 must begin with QUIC Initial, not EasyTier framing"
                    );
                    assert!(observed.initial_datagrams > 0);
                    assert_eq!(observed.nonstandard_initial_versions, 0);
                    assert_eq!(observed.easytier_envelopes, 0);
                    assert_eq!(observed.plaintext_markers, 0);
                    println!(
                        "native HTTP3 {listener_scheme} {bind}: 512 packets, {} IP bytes, \
                         {} wire datagrams, {} wire bytes, {:.2} Mbps in {:.3}s",
                        sent + replied,
                        observed.datagrams,
                        observed.bytes,
                        (sent + replied) as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000.0,
                        elapsed.as_secs_f64(),
                    );
                    drop(observed);
                    client.stop().await;
                    server.stop().await;
                    relay.abort();
                    assert!(relay.await.unwrap_err().is_cancelled());
                }
            })
            .await
            .expect("native HTTP3 wire and burst parity test timed out");
        }

        #[rstest::rstest]
        #[tokio::test(flavor = "multi_thread")]
        async fn native_core_udp_listener_round_trips_raw_udp_and_http3_on_one_port(
            #[values("127.0.0.1:0", "[::1]:0")] bind: &str,
        ) {
            tokio::time::timeout(Duration::from_secs(45), async {
                let network = "native-dual-udp-http3";
                let (server, mut server_packets) = build_instance(
                    network,
                    "10.252.0.1/24",
                    vec![format!("udp://{bind}").parse().unwrap()],
                    false,
                );
                let (raw, mut raw_packets) =
                    build_instance(network, "10.252.0.2/24", Vec::new(), false);
                let (http3, mut http3_packets) =
                    build_instance(network, "10.252.0.3/24", Vec::new(), false);
                let (start_server, start_raw, start_http3) =
                    tokio::join!(server.start(), raw.start(), http3.start());
                start_server.unwrap();
                start_raw.unwrap();
                start_http3.unwrap();

                let listeners = server.running_listeners();
                assert!(!listeners.iter().any(|url| url.scheme() == "http3"));
                let udp_listeners = listeners
                    .into_iter()
                    .filter(|url| url.scheme() == "udp")
                    .collect::<Vec<_>>();
                assert_eq!(udp_listeners.len(), 1);
                let udp_url = udp_listeners[0].clone();
                let mut http3_url = udp_url.clone();
                http3_url.set_scheme("http3").unwrap();
                raw.add_connector(udp_url.clone()).unwrap();
                http3.add_connector(http3_url.clone()).unwrap();

                wait_for_connection(&raw, server.peer_id(), "udp", true).await;
                let http3_tunnel =
                    wait_for_connection(&http3, server.peer_id(), "http3", true).await;
                let remote_url: url::Url = http3_tunnel.remote_addr.unwrap().into();
                assert_eq!(remote_url.scheme(), "http3");
                assert_eq!(remote_url.host(), http3_url.host());
                assert_eq!(remote_url.port(), http3_url.port());
                let accepted = wait_for_connection(&server, http3.peer_id(), "http3", true).await;
                let local_url: url::Url = accepted.local_addr.unwrap().into();
                assert_eq!(local_url.port(), udp_url.port());

                for (client, packets, client_ip, marker) in [
                    (&raw, &mut raw_packets, "10.252.0.2", b"raw UDP".as_slice()),
                    (
                        &http3,
                        &mut http3_packets,
                        "10.252.0.3",
                        b"HTTP3".as_slice(),
                    ),
                ] {
                    let request = ip_packet(
                        client_ip.parse().unwrap(),
                        "10.252.0.1".parse().unwrap(),
                        marker,
                    );
                    deliver_packet(client, &mut server_packets, &request).await;
                    let reply = ip_packet(
                        "10.252.0.1".parse().unwrap(),
                        client_ip.parse().unwrap(),
                        marker,
                    );
                    deliver_packet(&server, packets, &reply).await;
                }
                assert!(server.connected_peers().await.contains(&raw.peer_id()));
                assert!(server.connected_peers().await.contains(&http3.peer_id()));

                http3.stop().await;
                raw.stop().await;
                server.stop().await;
            })
            .await
            .expect("native dual UDP/HTTP3 listener test timed out");
        }

        #[cfg(feature = "websocket")]
        #[rstest::rstest]
        #[tokio::test(flavor = "multi_thread")]
        async fn native_core_direct_upgrades_tcp_or_wss_to_http3_companion(
            #[values("127.0.0.1:0", "[::1]:0")] bind: &str,
            #[values("tcp", "wss")] bootstrap_scheme: &str,
        ) {
            tokio::time::timeout(Duration::from_secs(45), async {
                let network = "native-auto-http3-upgrade";
                let (server, mut server_packets) = build_instance(
                    network,
                    "10.253.0.1/24",
                    vec![
                        format!("{bootstrap_scheme}://{bind}").parse().unwrap(),
                        format!("udp://{bind}").parse().unwrap(),
                    ],
                    true,
                );
                let (client, mut client_packets) =
                    build_instance(network, "10.253.0.2/24", Vec::new(), true);
                let (start_server, start_client) = tokio::join!(server.start(), client.start());
                start_server.unwrap();
                start_client.unwrap();
                let listeners = server.running_listeners();
                assert!(!listeners.iter().any(|url| url.scheme() == "http3"));
                let bootstrap = listeners
                    .iter()
                    .find(|url| url.scheme() == bootstrap_scheme)
                    .unwrap()
                    .clone();
                let udp_url = listeners.iter().find(|url| url.scheme() == "udp").unwrap();
                client.add_connector(bootstrap).unwrap();
                wait_for_connection(&client, server.peer_id(), bootstrap_scheme, false).await;

                // The only connector is TCP/WSS, so HTTP3 must be discovered via peer RPC.
                let upgraded = wait_for_connection(&client, server.peer_id(), "http3", true).await;
                let remote_url: url::Url = upgraded.remote_addr.unwrap().into();
                assert_eq!(remote_url.scheme(), "http3");
                assert_eq!(remote_url.host(), udp_url.host());
                assert_eq!(remote_url.port(), udp_url.port());
                wait_for_connection(&server, client.peer_id(), "http3", true).await;
                assert_eq!(client.list_connectors().len(), 1);
                // Preference cleanup must retain the explicitly configured bootstrap path.
                wait_for_connection(&client, server.peer_id(), bootstrap_scheme, false).await;

                let request = ip_packet(
                    "10.253.0.2".parse().unwrap(),
                    "10.253.0.1".parse().unwrap(),
                    b"automatic HTTP3 request",
                );
                deliver_packet(&client, &mut server_packets, &request).await;
                let reply = ip_packet(
                    "10.253.0.1".parse().unwrap(),
                    "10.253.0.2".parse().unwrap(),
                    b"automatic HTTP3 reply",
                );
                deliver_packet(&server, &mut client_packets, &reply).await;
                client.stop().await;
                server.stop().await;
            })
            .await
            .expect("native automatic HTTP3 upgrade test timed out");
        }
    }
}
