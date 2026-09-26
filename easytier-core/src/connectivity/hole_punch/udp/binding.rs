//! Core-owned UDP hole-punch socket/session runtime.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Weak},
};

use anyhow::Context as _;
use async_trait::async_trait;

use crate::{
    connectivity::{
        direct::DirectConnectorHost,
        hole_punch::port_mapping::{UdpPortMappingPlatform, start_udp_port_mapping},
        hole_punch::{HolePunchRpcRegistry, HolePunchTunnelSink},
        protocol::{ClientProtocolUpgrader, ServerProtocolUpgrader},
        stun::{StunInfoProvider, StunSocketMapper},
    },
    proto::peer_rpc::UdpHolePunchRpcServer,
    socket::{
        IpVersion, ListenerConnectionCounter, SocketContext,
        tcp::VirtualTcpSocketFactory,
        udp::{
            UdpBindOptions, UdpSessionLayer, UdpSessionProtocol, UdpSessionSocket,
            UdpSessionStunResponder, VirtualUdpSocket, VirtualUdpSocketFactory,
        },
    },
};

use super::{
    ProtocolUdpHolePunchTransportSink, UdpHolePunchConnector, UdpHolePunchPeerSource,
    UdpHolePunchRuntime, UdpPunchAcceptor, UdpPunchListener, UdpPunchScheme, UdpPunchSocket,
    UdpResolvedPublicAddr, UdpSymPunchLock,
    rpc::{PeerRpcUdpHolePunchSignaling, UdpHolePunchRpcEndpoint, UdpHolePunchRpcSource},
};

async fn resolve_public_addr_with_policy<S>(
    stun: &dyn StunSocketMapper<S>,
    platform: Option<Arc<dyn UdpPortMappingPlatform>>,
    events: Arc<dyn crate::events::CoreEventSink>,
    socket: Arc<S>,
    local_listener: &url::Url,
    disable_upnp: bool,
) -> anyhow::Result<UdpResolvedPublicAddr>
where
    S: VirtualUdpSocket + 'static,
{
    let port_mapping_lease = if disable_upnp {
        None
    } else if let Some(platform) = platform {
        match start_udp_port_mapping(platform, events, local_listener).await {
            Ok(lease) => lease,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    %local_listener,
                    "failed to establish udp port mapping, fallback to stun-only public addr resolution"
                );
                None
            }
        }
    } else {
        None
    };

    let mapped_addr = stun
        .get_udp_port_mapping_with_socket(socket)
        .await
        .with_context(|| format!("resolve udp public addr for {local_listener}"))?;
    if let Some(lease) = &port_mapping_lease {
        lease.public_addr_resolved(mapped_addr);
    } else {
        tracing::debug!(
            %local_listener,
            stun_mapped_addr = %mapped_addr,
            "udp public addr resolved without port mapping"
        );
    }

    Ok(UdpResolvedPublicAddr {
        mapped_addr,
        port_mapping_lease,
    })
}

fn managed_local_addr_error(
    local_addr: SocketAddr,
    is_local_virtual_ipv4: bool,
    is_easytier_managed_ipv6: bool,
) -> Option<&'static str> {
    match local_addr.ip() {
        IpAddr::V4(_) if is_local_virtual_ipv4 => Some("local address is virtual ipv4"),
        IpAddr::V6(_) if is_easytier_managed_ipv6 => Some("local address is easytier-managed ipv6"),
        _ => None,
    }
}

type HostUdpSocket<H> = <H as VirtualUdpSocketFactory>::Socket;
type HostTcpSocket<H> = <H as VirtualTcpSocketFactory>::Socket;
type CoreUdpSessionLayer<H> = UdpSessionLayer<HostUdpSocket<H>, H>;
type CoreUdpHolePunchTransportSink<H, P> = ProtocolUdpHolePunchTransportSink<HostTcpSocket<H>, P>;
type CoreUdpHolePunchConnector<H, P> = UdpHolePunchConnector<
    P,
    PeerRpcUdpHolePunchSignaling<P>,
    CoreUdpHolePunchTransportSink<H, P>,
    CoreUdpHolePunchRuntime<H, P>,
>;
type CoreUdpHolePunchEndpoint<H, P> =
    UdpHolePunchRpcEndpoint<CoreUdpHolePunchRuntime<H, P>, CoreUdpHolePunchTransportSink<H, P>>;

pub(crate) struct UdpHolePunchProtocols<TcpSocket> {
    pub client: Arc<dyn ClientProtocolUpgrader<TcpSocket>>,
    pub server: Arc<dyn ServerProtocolUpgrader<TcpSocket>>,
}

pub(crate) struct CoreUdpHolePunchService<H, P>
where
    H: DirectConnectorHost,
    P: UdpHolePunchPeerSource
        + HolePunchTunnelSink
        + UdpHolePunchRpcSource
        + HolePunchRpcRegistry
        + 'static,
{
    server: Arc<CoreUdpHolePunchEndpoint<H, P>>,
    client: CoreUdpHolePunchConnector<H, P>,
    peer_source: Arc<P>,
    http3_supported: bool,
}

impl<H, P> CoreUdpHolePunchService<H, P>
where
    H: DirectConnectorHost + Send + Sync + 'static,
    HostUdpSocket<H>: VirtualUdpSocket + 'static,
    P: UdpHolePunchPeerSource
        + HolePunchTunnelSink
        + UdpHolePunchRpcSource
        + HolePunchRpcRegistry
        + 'static,
{
    pub(crate) fn new(
        peer_source: Arc<P>,
        host: Arc<H>,
        stun: Arc<dyn StunSocketMapper<HostUdpSocket<H>>>,
        platform: Option<Arc<dyn UdpPortMappingPlatform>>,
        events: Arc<dyn crate::events::CoreEventSink>,
        socket_context: SocketContext,
        protocols: UdpHolePunchProtocols<HostTcpSocket<H>>,
    ) -> Self {
        let UdpHolePunchProtocols {
            client: protocol,
            server: server_protocol,
        } = protocols;
        let stun_mapper = stun.clone();
        let stun_info: Arc<dyn StunInfoProvider> = stun;
        let supports_http3 =
            protocol.supports_scheme("http3") && server_protocol.supports_scheme("http3");
        let transport_sink = Arc::new(if supports_http3 {
            ProtocolUdpHolePunchTransportSink::with_http3_server(
                protocol,
                server_protocol,
                peer_source.clone(),
            )
        } else {
            ProtocolUdpHolePunchTransportSink::new(protocol, peer_source.clone())
        });
        // HTTP3 is negotiated per punch. Peers on the official mainline omit
        // the negotiation field, so they still receive the raw-UDP protocol.
        let runtime = Arc::new(CoreUdpHolePunchRuntime::new(
            host,
            peer_source.clone(),
            stun_mapper,
            platform,
            events,
            socket_context,
        ));
        let sym_punch_lock = UdpSymPunchLock::default();
        let policy = peer_source.p2p_policy_flags();
        let client = UdpHolePunchConnector::new(
            peer_source.clone(),
            Arc::new(PeerRpcUdpHolePunchSignaling::new(peer_source.clone())),
            transport_sink.clone(),
            runtime.clone(),
            stun_info.clone(),
            sym_punch_lock.clone(),
            Some(peer_source.p2p_demand_notify()),
        );

        Self {
            server: UdpHolePunchRpcEndpoint::new(
                stun_info,
                transport_sink,
                sym_punch_lock,
                runtime,
                peer_source.clone(),
                policy.only_use_wss_http3_for_hole_punching,
                policy.disable_wss_http3_for_p2p,
            ),
            client,
            peer_source,
            http3_supported: supports_http3,
        }
    }

    pub(crate) async fn start(&self) -> anyhow::Result<()> {
        let policy = self.peer_source.p2p_policy_flags();
        if policy.disable_udp_hole_punching {
            return Ok(());
        }
        // Under the only-WSS/HTTP3 policy UDP punching is still possible,
        // but every punch must upgrade to HTTP3; without that capability
        // there is no compliant UDP transport left.
        if policy.only_use_wss_http3_for_hole_punching && !self.http3_supported {
            tracing::warn!("HTTP3 hole punching unavailable: this runtime lacks HTTP3 support");
            return Ok(());
        }
        self.server.start().await;
        self.peer_source
            .register_rpc_service(UdpHolePunchRpcServer::new(Arc::downgrade(&self.server)));
        self.client.run_as_client();
        Ok(())
    }

    pub(crate) async fn stop(&self) {
        self.client.stop().await;
        self.server.begin_stop();
        self.peer_source
            .unregister_rpc_service(UdpHolePunchRpcServer::new(Arc::downgrade(&self.server)));
        self.server.stop().await;
    }
}

struct CoreUdpPunchAcceptor<H>
where
    H: VirtualUdpSocketFactory,
    HostUdpSocket<H>: VirtualUdpSocket,
{
    layer: Arc<CoreUdpSessionLayer<H>>,
    scheme: UdpPunchScheme,
}

#[async_trait]
impl<H> UdpPunchAcceptor for CoreUdpPunchAcceptor<H>
where
    H: VirtualUdpSocketFactory + UdpSessionStunResponder<HostUdpSocket<H>> + Send + Sync + 'static,
    HostUdpSocket<H>: VirtualUdpSocket + 'static,
{
    async fn accept(&mut self) -> anyhow::Result<UdpPunchSocket> {
        let session = if self.scheme == UdpPunchScheme::Http3 {
            self.layer
                .accept_classified_session(UdpSessionProtocol::Quic)
                .await?
        } else {
            self.layer.accept().await?
        };
        let remote_addr = session.peer_addr()?;
        Ok(UdpPunchSocket::new_with_scheme(
            session,
            remote_addr,
            self.layer.clone(),
            self.scheme.as_str(),
        ))
    }
}

struct CoreUdpPunchConnCounter<H>
where
    H: VirtualUdpSocketFactory,
    HostUdpSocket<H>: VirtualUdpSocket,
{
    layer: Weak<CoreUdpSessionLayer<H>>,
}

impl<H> std::fmt::Debug for CoreUdpPunchConnCounter<H>
where
    H: VirtualUdpSocketFactory,
    HostUdpSocket<H>: VirtualUdpSocket,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CoreUdpPunchConnCounter")
            .finish_non_exhaustive()
    }
}

impl<H> ListenerConnectionCounter for CoreUdpPunchConnCounter<H>
where
    H: VirtualUdpSocketFactory + UdpSessionStunResponder<HostUdpSocket<H>> + Send + Sync + 'static,
    HostUdpSocket<H>: VirtualUdpSocket + 'static,
{
    fn get(&self) -> Option<u32> {
        Some(
            self.layer
                .upgrade()
                .map(|layer| {
                    (layer.active_session_count() + layer.active_classified_session_count()) as u32
                })
                .unwrap_or(0),
        )
    }
}

pub struct CoreUdpHolePunchRuntime<H, P>
where
    H: DirectConnectorHost,
    P: UdpHolePunchPeerSource + 'static,
{
    host: Arc<H>,
    peer_source: Arc<P>,
    stun: Arc<dyn StunSocketMapper<HostUdpSocket<H>>>,
    platform: Option<Arc<dyn UdpPortMappingPlatform>>,
    events: Arc<dyn crate::events::CoreEventSink>,
    socket_context: SocketContext,
}

impl<H, P> CoreUdpHolePunchRuntime<H, P>
where
    H: DirectConnectorHost + Send + Sync + 'static,
    HostUdpSocket<H>: VirtualUdpSocket + 'static,
    P: UdpHolePunchPeerSource + 'static,
{
    pub fn new(
        host: Arc<H>,
        peer_source: Arc<P>,
        stun: Arc<dyn StunSocketMapper<HostUdpSocket<H>>>,
        platform: Option<Arc<dyn UdpPortMappingPlatform>>,
        events: Arc<dyn crate::events::CoreEventSink>,
        socket_context: SocketContext,
    ) -> Self {
        Self {
            host,
            peer_source,
            stun,
            platform,
            events,
            socket_context,
        }
    }

    fn session_layer(&self, socket: Arc<HostUdpSocket<H>>) -> Arc<CoreUdpSessionLayer<H>> {
        Arc::new(UdpSessionLayer::new_with_stun_responder(
            socket,
            self.host.clone(),
        ))
    }

    async fn create_listener_with_mapping(
        &self,
        resolve_public_addr: bool,
        port: Option<u16>,
        scheme: UdpPunchScheme,
    ) -> anyhow::Result<UdpPunchListener<HostUdpSocket<H>>> {
        let bind = match port {
            Some(port) => UdpBindOptions::hole_punch_candidate().with_local_addr(Some(
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)),
            )),
            None => UdpBindOptions::hole_punch_control(),
        }
        .with_context(self.socket_context.clone().with_ip_version(IpVersion::V4));
        let socket = self.host.bind_udp(bind).await?;
        let local_port = socket.local_addr()?.port();
        let resolved = if resolve_public_addr {
            self.resolve_public_addr(socket.clone()).await?
        } else {
            UdpResolvedPublicAddr {
                mapped_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, local_port)),
                port_mapping_lease: None,
            }
        };

        let layer = self.session_layer(socket.clone());
        if scheme == UdpPunchScheme::Http3 {
            // Queue the first QUIC Initial even before the accept task is polled.
            layer.enable_classified_accept(UdpSessionProtocol::Quic)?;
        }
        let conn_counter = Arc::new(CoreUdpPunchConnCounter {
            layer: Arc::downgrade(&layer),
        });
        let acceptor = Box::new(CoreUdpPunchAcceptor { layer, scheme });

        Ok(UdpPunchListener {
            socket,
            mapped_addr: resolved.mapped_addr,
            conn_counter,
            acceptor,
            port_mapping_lease: resolved.port_mapping_lease,
            scheme,
        })
    }

    async fn resolve_public_addr(
        &self,
        socket: Arc<HostUdpSocket<H>>,
    ) -> anyhow::Result<UdpResolvedPublicAddr> {
        let local_port = socket.local_addr()?.port();
        let local_listener: url::Url = format!("udp://0.0.0.0:{local_port}").parse()?;
        resolve_public_addr_with_policy(
            self.stun.as_ref(),
            self.platform.clone(),
            self.events.clone(),
            socket,
            &local_listener,
            self.peer_source.p2p_policy_flags().disable_upnp,
        )
        .await
    }

    async fn validate_socket_route(
        &self,
        context: SocketContext,
        remote_addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let local_addr = self
            .host
            .local_addr_for_remote(remote_addr, context)
            .await?;
        let is_local_virtual_ipv4 = match local_addr.ip() {
            IpAddr::V4(ip) => self.peer_source.is_local_virtual_ip(&IpAddr::V4(ip)),
            IpAddr::V6(_) => false,
        };
        let is_easytier_managed_ipv6 = match local_addr.ip() {
            IpAddr::V4(_) => false,
            IpAddr::V6(ip) => self.peer_source.is_easytier_managed_ipv6(&ip).await,
        };
        if let Some(error) =
            managed_local_addr_error(local_addr, is_local_virtual_ipv4, is_easytier_managed_ipv6)
        {
            anyhow::bail!(error);
        }
        Ok(())
    }
}

#[async_trait]
impl<H, P> UdpHolePunchRuntime for CoreUdpHolePunchRuntime<H, P>
where
    H: DirectConnectorHost + Send + Sync + 'static,
    HostUdpSocket<H>: VirtualUdpSocket + 'static,
    P: UdpHolePunchPeerSource + 'static,
{
    type Socket = HostUdpSocket<H>;

    fn socket_context(&self) -> SocketContext {
        self.socket_context.clone()
    }

    async fn bind_udp(&self, options: UdpBindOptions) -> anyhow::Result<Arc<Self::Socket>> {
        self.host.bind_udp(options).await
    }

    async fn bind_direct_connect_udp(&self) -> anyhow::Result<Arc<Self::Socket>> {
        self.host
            .bind_udp(
                UdpBindOptions::hole_punch_candidate()
                    .with_context(self.socket_context.clone().with_ip_version(IpVersion::V4)),
            )
            .await
    }

    async fn resolve_udp_public_addr(
        &self,
        socket: Arc<Self::Socket>,
    ) -> anyhow::Result<UdpResolvedPublicAddr> {
        self.resolve_public_addr(socket).await
    }

    async fn create_listener(
        &self,
        _prefer_port_mapping: bool,
        scheme: UdpPunchScheme,
    ) -> anyhow::Result<UdpPunchListener<Self::Socket>> {
        self.create_listener_with_mapping(true, None, scheme).await
    }

    async fn create_port_bound_listener(
        &self,
        port: u16,
        scheme: UdpPunchScheme,
    ) -> anyhow::Result<UdpPunchListener<Self::Socket>> {
        self.create_listener_with_mapping(false, Some(port), scheme)
            .await
    }

    async fn connect_with_socket(
        &self,
        socket: Arc<Self::Socket>,
        remote: SocketAddr,
        scheme: UdpPunchScheme,
    ) -> anyhow::Result<UdpPunchSocket> {
        self.validate_socket_route(socket.socket_context(), remote)
            .await?;
        let layer = self.session_layer(socket);
        let session = if scheme == UdpPunchScheme::Http3 {
            layer.open_classified_session(UdpSessionProtocol::Quic, remote)?
        } else {
            layer.connect(remote).await?
        };
        if session.peer_addr()? != remote {
            tracing::debug!(
                recv_addr = ?session.peer_addr()?,
                ?remote,
                "udp connect addr not match"
            );
        }
        Ok(UdpPunchSocket::new_with_scheme(
            session,
            remote,
            layer,
            scheme.as_str(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::{Ipv6Addr, SocketAddrV6},
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use crate::{
        connectivity::stun::StunInfoProvider, packet::UDP_TUNNEL_HEADER_SIZE,
        proto::common::StunInfo, socket::udp::UdpSessionKind,
    };

    use super::*;

    #[derive(Debug)]
    struct MockSocket {
        local_addr: SocketAddr,
    }

    #[async_trait]
    impl VirtualUdpSocket for MockSocket {
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.local_addr)
        }

        async fn send_to(&self, data: &[u8], _addr: SocketAddr) -> io::Result<usize> {
            Ok(data.len())
        }

        async fn recv_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
            std::future::pending().await
        }
    }

    struct WireSocket {
        local_addr: SocketAddr,
        incoming: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>>,
        peer: tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
        sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    }

    impl WireSocket {
        fn pair() -> (Arc<Self>, Arc<Self>) {
            let (client_tx, client_rx) = tokio::sync::mpsc::unbounded_channel();
            let (server_tx, server_rx) = tokio::sync::mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    local_addr: "127.0.0.1:31000".parse().unwrap(),
                    incoming: tokio::sync::Mutex::new(client_rx),
                    peer: server_tx,
                    sent: Mutex::new(Vec::new()),
                }),
                Arc::new(Self {
                    local_addr: "127.0.0.1:31001".parse().unwrap(),
                    incoming: tokio::sync::Mutex::new(server_rx),
                    peer: client_tx,
                    sent: Mutex::new(Vec::new()),
                }),
            )
        }
    }

    #[async_trait]
    impl VirtualUdpSocket for WireSocket {
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.local_addr)
        }

        async fn send_to(&self, data: &[u8], addr: SocketAddr) -> io::Result<usize> {
            self.sent.lock().unwrap().push((data.to_vec(), addr));
            self.peer
                .send((data.to_vec(), self.local_addr))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "wire peer closed"))?;
            Ok(data.len())
        }

        async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
            let (data, addr) =
                self.incoming.lock().await.recv().await.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "wire peer closed")
                })?;
            let len = data.len().min(buf.len());
            buf[..len].copy_from_slice(&data[..len]);
            Ok((len, addr))
        }
    }

    struct WireHost;

    #[async_trait]
    impl VirtualUdpSocketFactory for WireHost {
        type Socket = WireSocket;

        async fn bind_udp(&self, _options: UdpBindOptions) -> anyhow::Result<Arc<Self::Socket>> {
            anyhow::bail!("wire sockets are prebound")
        }
    }

    fn wire_layer(socket: Arc<WireSocket>) -> Arc<CoreUdpSessionLayer<WireHost>> {
        Arc::new(UdpSessionLayer::new_with_stun_responder(
            socket,
            Arc::new(WireHost),
        ))
    }

    #[tokio::test]
    async fn native_http3_punch_preserves_wire_bytes_counter_and_layer_lifetime() {
        let (client_socket, server_socket) = WireSocket::pair();
        let client_layer = wire_layer(client_socket.clone());
        let server_layer = wire_layer(server_socket.clone());
        server_layer
            .enable_classified_accept(UdpSessionProtocol::Quic)
            .unwrap();
        let server_weak = Arc::downgrade(&server_layer);
        let counter = CoreUdpPunchConnCounter::<WireHost> {
            layer: server_weak.clone(),
        };
        assert_eq!(counter.get(), Some(0));
        let client = client_layer
            .open_classified_session(UdpSessionProtocol::Quic, server_socket.local_addr)
            .unwrap();
        assert_eq!(client.kind(), UdpSessionKind::Quic);
        assert!(client_socket.sent.lock().unwrap().is_empty());

        // A valid QUIC v1 Initial prefix; padding satisfies the 1200-byte minimum.
        let mut initial = vec![0xc0, 0, 0, 0, 1, 8, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 1, 0];
        initial.resize(1200, 0);
        client.send(&initial).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while counter.get() != Some(1) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let mut acceptor = CoreUdpPunchAcceptor::<WireHost> {
            layer: server_layer.clone(),
            scheme: UdpPunchScheme::Http3,
        };
        let punched = tokio::time::timeout(Duration::from_secs(1), acceptor.accept())
            .await
            .unwrap()
            .unwrap();
        let (connected, url) = punched.into_connected();
        assert_eq!(url.scheme(), "http3");
        let (mut server, guard) = connected.into_parts();
        assert_eq!(server.kind(), UdpSessionKind::Quic);
        server.keep_layer_alive(guard);
        drop(acceptor);
        drop(server_layer);
        assert!(server_weak.upgrade().is_some());
        assert_eq!(counter.get(), Some(1));

        let mut buf = vec![0; 1500];
        let len = server.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], initial.as_slice());
        assert_eq!(
            client_socket.sent.lock().unwrap().as_slice(),
            [(initial, server_socket.local_addr)]
        );
        for index in 0..64u8 {
            let datagram = vec![0x40, index, 0xa5, 0x5a];
            client.send(&datagram).await.unwrap();
            let len = tokio::time::timeout(Duration::from_secs(1), server.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..len], datagram.as_slice());
            server.send(&datagram).await.unwrap();
            let len = tokio::time::timeout(Duration::from_secs(1), client.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..len], datagram.as_slice());
        }
        assert!(
            server_socket
                .sent
                .lock()
                .unwrap()
                .iter()
                .all(|(datagram, _)| datagram.len() == 4 && datagram[0] == 0x40)
        );
        drop(server);
        assert!(server_weak.upgrade().is_none());
        assert_eq!(counter.get(), Some(0));
        drop(client);
        assert_eq!(client_layer.active_classified_session_count(), 0);
    }

    #[tokio::test]
    async fn raw_and_legacy_http3_punch_keep_easytier_mux_wire_protocol() {
        for scheme in [UdpPunchScheme::Udp, UdpPunchScheme::Http3Mux] {
            let (client_socket, server_socket) = WireSocket::pair();
            let client_layer = wire_layer(client_socket.clone());
            let server_layer = wire_layer(server_socket.clone());
            let mut acceptor = CoreUdpPunchAcceptor::<WireHost> {
                layer: server_layer.clone(),
                scheme,
            };
            let (client, accepted) = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(
                    client_layer.connect(server_socket.local_addr),
                    acceptor.accept()
                )
            })
            .await
            .unwrap();
            let client = client.unwrap();
            let (connected, url) = accepted.unwrap().into_connected();
            let (server, _guard) = connected.into_parts();
            assert_eq!(url.scheme(), scheme.as_str());
            assert_eq!(client.kind(), UdpSessionKind::EasyTierMux);
            assert_eq!(server.kind(), UdpSessionKind::EasyTierMux);
            let counter = CoreUdpPunchConnCounter::<WireHost> {
                layer: Arc::downgrade(&server_layer),
            };
            assert_eq!(counter.get(), Some(1));

            let payload = b"mux-payload";
            client.send(payload).await.unwrap();
            let mut buf = [0; 64];
            let len = tokio::time::timeout(Duration::from_secs(1), server.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..len], payload);
            let sent = client_socket.sent.lock().unwrap();
            let datagram = &sent.last().unwrap().0;
            assert_eq!(datagram.len(), payload.len() + UDP_TUNNEL_HEADER_SIZE);
            assert_eq!(&datagram[UDP_TUNNEL_HEADER_SIZE..], payload);
            drop(sent);
            drop(server);
            assert_eq!(counter.get(), Some(0));
            drop(client);
            assert_eq!(client_layer.active_session_count(), 0);
        }
    }

    struct MockStun {
        mapped_addr: SocketAddr,
        fail: bool,
        calls: AtomicUsize,
    }

    impl MockStun {
        fn succeeds_with(mapped_addr: SocketAddr) -> Self {
            Self {
                mapped_addr,
                fail: false,
                calls: AtomicUsize::new(0),
            }
        }

        fn failing() -> Self {
            Self {
                mapped_addr: "0.0.0.0:0".parse().unwrap(),
                fail: true,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl StunInfoProvider for MockStun {
        fn get_stun_info(&self) -> StunInfo {
            StunInfo::default()
        }

        async fn get_udp_port_mapping(&self, _local_port: u16) -> anyhow::Result<SocketAddr> {
            Ok(self.mapped_addr)
        }

        async fn get_tcp_port_mapping(&self, _local_port: u16) -> anyhow::Result<SocketAddr> {
            Ok(self.mapped_addr)
        }

        fn update_stun_info(&self) {}
    }

    #[async_trait]
    impl StunSocketMapper<MockSocket> for MockStun {
        async fn get_udp_port_mapping_with_socket(
            &self,
            _socket: Arc<MockSocket>,
        ) -> anyhow::Result<SocketAddr> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("mock STUN failure");
            }
            Ok(self.mapped_addr)
        }
    }

    #[derive(Debug, Default)]
    struct LeaseState {
        drops: AtomicUsize,
    }

    #[derive(Default)]
    struct MockEvents {
        established: Mutex<Vec<crate::events::CoreEvent>>,
    }

    impl crate::events::CoreEventSink for MockEvents {
        fn emit(&self, event: crate::events::CoreEvent) {
            self.established.lock().unwrap().push(event);
        }
    }

    #[derive(Debug)]
    struct MockMapping {
        state: Arc<LeaseState>,
        backend: crate::connectivity::hole_punch::port_mapping::UdpPortMappingBackend,
    }

    #[async_trait]
    impl crate::connectivity::hole_punch::port_mapping::ActiveUdpPortMapping for MockMapping {
        fn backend(&self) -> crate::connectivity::hole_punch::port_mapping::UdpPortMappingBackend {
            self.backend
        }

        fn local_addr(&self) -> SocketAddr {
            "192.168.1.5:30123".parse().unwrap()
        }

        fn gateway_external_port(&self) -> u16 {
            40123
        }

        async fn renew(&self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn remove(&self) -> anyhow::Result<()> {
            self.state.drops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct MockPlatform {
        calls: AtomicUsize,
        fail: bool,
        lease_state: Arc<LeaseState>,
    }

    impl MockPlatform {
        fn failing() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: true,
                lease_state: Arc::default(),
            }
        }

        fn with_lease(lease_state: Arc<LeaseState>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: false,
                lease_state,
            }
        }
    }

    #[async_trait]
    impl UdpPortMappingPlatform for MockPlatform {
        async fn establish_udp_port_mapping(
            &self,
            backend: crate::connectivity::hole_punch::port_mapping::UdpPortMappingBackend,
            _local_listener: &url::Url,
        ) -> Result<
            Box<dyn crate::connectivity::hole_punch::port_mapping::ActiveUdpPortMapping>,
            crate::connectivity::hole_punch::port_mapping::UdpPortMappingAttemptError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(
                    crate::connectivity::hole_punch::port_mapping::UdpPortMappingAttemptError::establishment(
                        anyhow::anyhow!("mock port-mapping failure"),
                    ),
                );
            }
            Ok(Box::new(MockMapping {
                state: self.lease_state.clone(),
                backend,
            }))
        }
    }

    fn socket() -> Arc<MockSocket> {
        Arc::new(MockSocket {
            local_addr: "0.0.0.0:30123".parse().unwrap(),
        })
    }

    fn listener_url() -> url::Url {
        "udp://0.0.0.0:30123".parse().unwrap()
    }

    #[tokio::test]
    async fn port_mapping_failure_falls_back_to_stun() {
        let mapped_addr = "198.51.100.8:40123".parse().unwrap();
        let stun = MockStun::succeeds_with(mapped_addr);
        let platform = Arc::new(MockPlatform::failing());

        let resolved = resolve_public_addr_with_policy(
            &stun,
            Some(platform.clone()),
            Arc::new(()),
            socket(),
            &listener_url(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(resolved.mapped_addr, mapped_addr);
        assert!(resolved.port_mapping_lease.is_none());
        assert_eq!(platform.calls.load(Ordering::SeqCst), 2);
        assert_eq!(stun.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stun_failure_releases_mapping_without_notification() {
        let lease_state = Arc::new(LeaseState::default());
        let platform = Arc::new(MockPlatform::with_lease(lease_state.clone()));
        let events = Arc::new(MockEvents::default());

        let result = resolve_public_addr_with_policy(
            &MockStun::failing(),
            Some(platform),
            events.clone(),
            socket(),
            &listener_url(),
            false,
        )
        .await;

        assert!(result.is_err());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while lease_state.drops.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(lease_state.drops.load(Ordering::SeqCst), 1);
        assert!(events.established.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disable_upnp_is_applied_per_resolution() {
        let mapped_addr = "198.51.100.9:40124".parse().unwrap();
        let stun = MockStun::succeeds_with(mapped_addr);
        let lease_state = Arc::new(LeaseState::default());
        let platform = Arc::new(MockPlatform::with_lease(lease_state));

        let disabled = resolve_public_addr_with_policy(
            &stun,
            Some(platform.clone()),
            Arc::new(()),
            socket(),
            &listener_url(),
            true,
        )
        .await
        .unwrap();
        assert!(disabled.port_mapping_lease.is_none());
        assert_eq!(platform.calls.load(Ordering::SeqCst), 0);

        let enabled = resolve_public_addr_with_policy(
            &stun,
            Some(platform.clone()),
            Arc::new(()),
            socket(),
            &listener_url(),
            false,
        )
        .await
        .unwrap();
        assert!(enabled.port_mapping_lease.is_some());
        assert_eq!(platform.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn successful_mapping_is_notified_and_held_with_result() {
        let mapped_addr = "198.51.100.10:40125".parse().unwrap();
        let lease_state = Arc::new(LeaseState::default());
        let platform = Arc::new(MockPlatform::with_lease(lease_state.clone()));
        let events = Arc::new(MockEvents::default());

        let resolved = resolve_public_addr_with_policy(
            &MockStun::succeeds_with(mapped_addr),
            Some(platform),
            events.clone(),
            socket(),
            &listener_url(),
            false,
        )
        .await
        .unwrap();

        {
            let established = events.established.lock().unwrap();
            assert!(matches!(
                established.as_slice(),
                [crate::events::CoreEvent::UdpPortMappingEstablished {
                    local_listener,
                    mapped_listener,
                    backend,
                }] if local_listener == &listener_url()
                    && mapped_listener.as_str() == "udp://198.51.100.10:40125"
                    && backend == "igd"
            ));
        }
        assert_eq!(lease_state.drops.load(Ordering::SeqCst), 0);
        drop(resolved);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while lease_state.drops.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(lease_state.drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn managed_local_addresses_are_rejected() {
        let virtual_ipv4 = "10.144.0.2:1234".parse().unwrap();
        assert_eq!(
            managed_local_addr_error(virtual_ipv4, true, false),
            Some("local address is virtual ipv4")
        );
        assert_eq!(managed_local_addr_error(virtual_ipv4, false, false), None);

        let managed_ipv6 = SocketAddr::V6(SocketAddrV6::new(
            "fd00::1".parse::<Ipv6Addr>().unwrap(),
            1234,
            0,
            0,
        ));
        assert_eq!(
            managed_local_addr_error(managed_ipv6, false, true),
            Some("local address is easytier-managed ipv6")
        );
        assert_eq!(managed_local_addr_error(managed_ipv6, false, false), None);
    }
}
