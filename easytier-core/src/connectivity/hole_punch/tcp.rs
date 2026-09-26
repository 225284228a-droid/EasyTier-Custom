#![cfg_attr(not(feature = "tcp-hole-punch"), allow(dead_code))]

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::Context as _;
use async_trait::async_trait;
use dashmap::DashMap;
use quanta::Instant;
use rand::Rng as _;
use tokio::task::JoinSet;
use tokio_util::task::AbortOnDropHandle;

use crate::{
    config::{P2pPolicyFlags, PeerDisguiseP2pFlags, PeerId},
    connectivity::{
        hole_punch::{
            HolePunchRpcRegistry, HolePunchTunnelSink,
            policy::{
                BackOff, should_accept_inbound_punch, should_background_p2p_with_peer,
                should_try_p2p_with_peer,
            },
        },
        protocol::{ClientProtocolUpgrader, ServerProtocolUpgrade, ServerProtocolUpgrader},
        stun::StunInfoProvider,
        transport::ConnectedTransport,
    },
    foundation::task::{
        ExternalTaskSignal, PeerTaskLauncher, PeerTaskManager, reap_joinset_background,
    },
    proto::{
        common::{NatType, PeerFeatureFlag},
        peer_rpc::{
            PortSequenceDirection, TcpHolePunchPredictedPorts, TcpHolePunchRequest,
            TcpHolePunchResponse, TcpHolePunchRpc, TcpHolePunchRpcServer,
        },
        rpc_types::{self, controller::BaseController, controller::Controller as _},
    },
    socket::{
        IpVersion, SocketContext,
        tcp::{
            TcpBindOptions, TcpConnectOptions, TcpListenOptions, VirtualTcpListener,
            VirtualTcpListenerFactory, VirtualTcpSocketFactory,
        },
    },
};

pub trait TcpHolePunchHost: VirtualTcpListenerFactory + VirtualTcpSocketFactory {}

impl<T> TcpHolePunchHost for T where T: VirtualTcpListenerFactory + VirtualTcpSocketFactory {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpHolePunchAdmission {
    Client,
    Server,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpPunchCandidate {
    pub peer_id: PeerId,
    pub tcp_nat_type: NatType,
    pub feature_flag: Option<PeerFeatureFlag>,
    pub has_direct_connection: bool,
    pub has_disguised_connection: bool,
    pub has_recent_traffic: bool,
    pub peer_disguise_flags: PeerDisguiseP2pFlags,
}

/// Narrow peer-graph view required by the TCP hole-punch engine.
///
/// Implemented only by the sealed peer adapter in `super::peer_adapters`.
#[async_trait]
pub trait TcpHolePunchPeerSource: Send + Sync + 'static {
    fn local_peer_id(&self) -> PeerId;

    fn p2p_policy_flags(&self) -> P2pPolicyFlags;

    fn tcp_hole_punching_disabled(&self) -> bool;

    fn p2p_demand_notify(&self) -> Arc<ExternalTaskSignal>;

    async fn candidates(&self) -> Vec<TcpPunchCandidate>;

    fn rpc_stub(
        &self,
        dst_peer_id: PeerId,
    ) -> Box<dyn TcpHolePunchRpc<Controller = BaseController> + Send + Sync + 'static>;
}

#[derive(Debug, thiserror::Error)]
pub enum TcpHolePunchTransportError {
    #[error("TCP hole-punch protocol upgrade failed")]
    Upgrade(#[source] anyhow::Error),
    #[error("TCP hole-punch tunnel admission failed")]
    Admission(#[source] anyhow::Error),
}

#[async_trait]
pub trait TcpHolePunchTransportSink: Send + Sync + 'static {
    type ConnectedSocket;
    type AcceptedSocket;

    fn supports_wss_hole_punching(&self) -> bool;

    async fn add_connected_transport(
        &self,
        socket: Self::ConnectedSocket,
        requested_url: url::Url,
        admission: TcpHolePunchAdmission,
    ) -> Result<(), TcpHolePunchTransportError>;

    async fn add_accepted_transport(
        &self,
        socket: Self::AcceptedSocket,
        local_url: url::Url,
    ) -> Result<(), TcpHolePunchTransportError>;
}

pub struct ProtocolTcpHolePunchTransportSink<ConnectedSocket, AcceptedSocket, T> {
    client_protocol: Arc<dyn ClientProtocolUpgrader<ConnectedSocket>>,
    connected_server_protocol: Arc<dyn ServerProtocolUpgrader<ConnectedSocket>>,
    server_protocol: Arc<dyn ServerProtocolUpgrader<AcceptedSocket>>,
    tunnel_sink: Arc<T>,
}

impl<ConnectedSocket, AcceptedSocket, T>
    ProtocolTcpHolePunchTransportSink<ConnectedSocket, AcceptedSocket, T>
{
    pub fn new(
        client_protocol: Arc<dyn ClientProtocolUpgrader<ConnectedSocket>>,
        connected_server_protocol: Arc<dyn ServerProtocolUpgrader<ConnectedSocket>>,
        server_protocol: Arc<dyn ServerProtocolUpgrader<AcceptedSocket>>,
        tunnel_sink: Arc<T>,
    ) -> Self {
        Self {
            client_protocol,
            connected_server_protocol,
            server_protocol,
            tunnel_sink,
        }
    }
}

#[async_trait]
impl<ConnectedSocket, AcceptedSocket, T> TcpHolePunchTransportSink
    for ProtocolTcpHolePunchTransportSink<ConnectedSocket, AcceptedSocket, T>
where
    ConnectedSocket: Send + 'static,
    AcceptedSocket: Send + 'static,
    T: HolePunchTunnelSink,
{
    type ConnectedSocket = ConnectedSocket;
    type AcceptedSocket = AcceptedSocket;

    fn supports_wss_hole_punching(&self) -> bool {
        self.client_protocol.supports_scheme("wss")
            && self.connected_server_protocol.supports_scheme("wss")
            && self.server_protocol.supports_scheme("wss")
    }

    async fn add_connected_transport(
        &self,
        socket: ConnectedSocket,
        requested_url: url::Url,
        admission: TcpHolePunchAdmission,
    ) -> Result<(), TcpHolePunchTransportError> {
        if requested_url.scheme() == "wss" && admission == TcpHolePunchAdmission::Server {
            let local_url = local_server_url(&requested_url);
            let upgrade = self
                .connected_server_protocol
                .upgrade_tcp(socket, local_url)
                .await
                .map_err(TcpHolePunchTransportError::Upgrade)?;
            let ServerProtocolUpgrade::Tunnel(tunnel) = upgrade else {
                return Err(TcpHolePunchTransportError::Upgrade(anyhow::anyhow!(
                    "TCP hole-punch protocol returned a tunnel acceptor"
                )));
            };
            return self
                .tunnel_sink
                .add_server_tunnel(tunnel)
                .await
                .map_err(TcpHolePunchTransportError::Admission);
        }
        let tunnel = self
            .client_protocol
            .upgrade_client(ConnectedTransport::Tcp(socket), requested_url)
            .await
            .map_err(TcpHolePunchTransportError::Upgrade)?;
        match admission {
            TcpHolePunchAdmission::Client => self
                .tunnel_sink
                .add_client_tunnel(tunnel)
                .await
                .map_err(TcpHolePunchTransportError::Admission),
            TcpHolePunchAdmission::Server => self
                .tunnel_sink
                .add_server_tunnel(tunnel)
                .await
                .map_err(TcpHolePunchTransportError::Admission),
        }
    }

    async fn add_accepted_transport(
        &self,
        socket: AcceptedSocket,
        local_url: url::Url,
    ) -> Result<(), TcpHolePunchTransportError> {
        let upgrade = self
            .server_protocol
            .upgrade_tcp(socket, local_url)
            .await
            .map_err(TcpHolePunchTransportError::Upgrade)?;
        let ServerProtocolUpgrade::Tunnel(tunnel) = upgrade else {
            return Err(TcpHolePunchTransportError::Upgrade(anyhow::anyhow!(
                "TCP hole-punch protocol returned a tunnel acceptor"
            )));
        };
        self.tunnel_sink
            .add_server_tunnel(tunnel)
            .await
            .map_err(TcpHolePunchTransportError::Admission)
    }
}

type ConnectedTcpSocket<H> = <H as VirtualTcpSocketFactory>::Socket;
type AcceptedTcpSocket<H> =
    <<H as VirtualTcpListenerFactory>::Listener as VirtualTcpListener>::Socket;

pub(super) type TcpHolePunchTransportSinkFor<H> = dyn TcpHolePunchTransportSink<
        ConnectedSocket = ConnectedTcpSocket<H>,
        AcceptedSocket = AcceptedTcpSocket<H>,
    >;

fn bind_addr_for_port(port: u16, is_v6: bool) -> SocketAddr {
    if is_v6 {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)
    } else {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)
    }
}

fn local_server_url(requested_url: &url::Url) -> url::Url {
    format!(
        "{}://{}:{}",
        requested_url.scheme(),
        "0.0.0.0",
        requested_url.port().unwrap_or(0)
    )
    .parse()
    .expect("hole-punch local URL should be valid")
}

fn mapped_addr_url(scheme: &str, addr: SocketAddr) -> anyhow::Result<url::Url> {
    let url = match scheme {
        "tcp" => format!("tcp://{addr}"),
        "wss" => format!("wss://{addr}"),
        other => anyhow::bail!("unsupported TCP hole-punch scheme: {other}"),
    };
    url.parse().map_err(anyhow::Error::from)
}

pub async fn select_local_port<H>(
    host: &H,
    context: SocketContext,
    is_v6: bool,
) -> anyhow::Result<u16>
where
    H: VirtualTcpListenerFactory,
{
    let bind_addr = bind_addr_for_port(0, is_v6);
    tracing::trace!(?bind_addr, is_v6, "tcp hole punch select local port");
    let context = context.with_ip_version(if is_v6 { IpVersion::V6 } else { IpVersion::V4 });
    let listener = host
        .bind_tcp(
            TcpListenOptions::hole_punch(bind_addr).with_bind(
                TcpBindOptions::default()
                    .with_context(context)
                    .with_local_addr(Some(bind_addr)),
            ),
        )
        .await?;
    let port = listener.local_addr()?.port();
    tracing::debug!(?bind_addr, port, "tcp hole punch selected local port");
    Ok(port)
}

pub struct TcpHolePunchDialOptions<'a> {
    pub remote_mapped_addr: SocketAddr,
    pub local_port: u16,
    pub context: SocketContext,
    pub admission: TcpHolePunchAdmission,
    pub max_attempts: u32,
    pub scheme: &'a str,
}

// TCP supports simultaneous connect, so both peers may dial from the mapped port.
pub async fn try_connect_to_remote<H, AcceptedSocket>(
    host: Arc<H>,
    transport_sink: Arc<
        dyn TcpHolePunchTransportSink<
                ConnectedSocket = <H as VirtualTcpSocketFactory>::Socket,
                AcceptedSocket = AcceptedSocket,
            >,
    >,
    options: TcpHolePunchDialOptions<'_>,
) -> anyhow::Result<()>
where
    H: VirtualTcpSocketFactory,
    AcceptedSocket: 'static,
{
    let TcpHolePunchDialOptions {
        remote_mapped_addr,
        local_port,
        context,
        admission,
        max_attempts,
        scheme,
    } = options;
    tracing::info!(
        ?remote_mapped_addr,
        local_port,
        "tcp hole punch server start connect loop"
    );

    let bind_addr = bind_addr_for_port(local_port, remote_mapped_addr.is_ipv6());
    let context = context.with_ip_version(if remote_mapped_addr.is_ipv6() {
        IpVersion::V6
    } else {
        IpVersion::V4
    });
    let requested_url = mapped_addr_url(scheme, remote_mapped_addr)?;

    let start = crate::foundation::time::Instant::now();
    let mut attempts = 0_u32;
    while start.elapsed() < Duration::from_secs(10) && attempts < max_attempts {
        attempts = attempts.wrapping_add(1);
        let bind = hole_punch_bind_options(context.clone(), bind_addr);
        let options =
            TcpConnectOptions::hole_punch(remote_mapped_addr, Some(bind_addr)).with_bind(bind);
        if let Ok(Ok(socket)) =
            crate::foundation::time::timeout(Duration::from_secs(3), host.connect_tcp(options))
                .await
        {
            let admission_result = transport_sink
                .add_connected_transport(socket, requested_url.clone(), admission)
                .await;
            match admission_result {
                Ok(()) => {}
                Err(TcpHolePunchTransportError::Upgrade(error)) => return Err(error),
                Err(TcpHolePunchTransportError::Admission(error)) => {
                    tracing::error!(
                        ?remote_mapped_addr,
                        local_port,
                        attempts,
                        ?error,
                        "tcp hole punch server connected and added client tunnel failed"
                    );
                    continue;
                }
            }

            tracing::info!(
                ?remote_mapped_addr,
                local_port,
                attempts,
                ?admission,
                "tcp hole punch server connected and added tunnel"
            );
            return Ok(());
        }
        tracing::trace!(
            ?remote_mapped_addr,
            local_port,
            attempts,
            "tcp hole punch server connect attempt failed"
        );
        let sleep_ms = rand::thread_rng().gen_range(10..100);
        crate::foundation::time::sleep(Duration::from_millis(sleep_ms)).await;
    }

    tracing::warn!(
        ?remote_mapped_addr,
        local_port,
        attempts,
        "tcp hole punch server connect loop timeout"
    );

    Err(anyhow::anyhow!(
        "tcp hole punch server connect loop timeout"
    ))
}

/// Symmetric-side fan-out dialer: keeps `SYMMETRIC_RESPONDER_SOCKET_COUNT`
/// sockets from distinct local ports dialing the cone initiator's stable
/// connector address, so each of them owns one mapping in the NAT's port
/// sequence for the initiator to hit. Works without any remote cooperation,
/// which is what repairs punching against unpatched cone initiators.
async fn connect_as_symmetric_responder<H, AcceptedSocket>(
    host: Arc<H>,
    transport_sink: Arc<
        dyn TcpHolePunchTransportSink<
                ConnectedSocket = <H as VirtualTcpSocketFactory>::Socket,
                AcceptedSocket = AcceptedSocket,
            >,
    >,
    remote_mapped_addr: SocketAddr,
    local_port: u16,
    context: SocketContext,
    admission: TcpHolePunchAdmission,
    scheme: &str,
) -> anyhow::Result<()>
where
    H: VirtualTcpSocketFactory,
    AcceptedSocket: 'static,
{
    tracing::info!(
        ?remote_mapped_addr,
        local_port,
        sockets = SYMMETRIC_RESPONDER_SOCKET_COUNT,
        "tcp hole punch symmetric responder start fan-out dial loop"
    );

    let is_v6 = remote_mapped_addr.is_ipv6();
    let context = context.with_ip_version(if is_v6 { IpVersion::V6 } else { IpVersion::V4 });
    let requested_url = mapped_addr_url(scheme, remote_mapped_addr)?;

    let mut tasks = JoinSet::new();
    for index in 0..SYMMETRIC_RESPONDER_SOCKET_COUNT {
        // The first socket reuses the announced listener port; the rest let
        // the OS pick distinct local ports, each allocating a fresh mapping.
        let port = if index == 0 { local_port } else { 0 };
        let bind_addr = bind_addr_for_port(port, is_v6);
        let bind = hole_punch_bind_options(context.clone(), bind_addr);
        let options =
            TcpConnectOptions::hole_punch(remote_mapped_addr, Some(bind_addr)).with_bind(bind);
        let host = host.clone();
        let transport_sink = transport_sink.clone();
        let requested_url = requested_url.clone();
        tasks.spawn(async move {
            let start = crate::foundation::time::Instant::now();
            while start.elapsed() < SYMMETRIC_RESPONDER_HOLD {
                if let Ok(Ok(socket)) = crate::foundation::time::timeout(
                    SYMMETRIC_RESPONDER_DIAL_TIMEOUT,
                    host.connect_tcp(options.clone()),
                )
                .await
                {
                    let admission_result = transport_sink
                        .add_connected_transport(socket, requested_url.clone(), admission)
                        .await;
                    match admission_result {
                        Ok(()) => {}
                        Err(TcpHolePunchTransportError::Upgrade(error)) => return Err(error),
                        Err(TcpHolePunchTransportError::Admission(error)) => {
                            tracing::warn!(
                                ?remote_mapped_addr,
                                port,
                                ?error,
                                "tcp hole punch symmetric responder tunnel admission failed"
                            );
                            continue;
                        }
                    }

                    tracing::info!(
                        ?remote_mapped_addr,
                        port,
                        "tcp hole punch symmetric responder connected and added tunnel"
                    );
                    return Ok(());
                }
                let sleep_ms = rand::thread_rng().gen_range(50..150);
                crate::foundation::time::sleep(Duration::from_millis(sleep_ms)).await;
            }
            Err(anyhow::anyhow!(
                "tcp hole punch symmetric responder dial loop timeout"
            ))
        });
    }

    while let Some(result) = tasks.join_next().await {
        if let Ok(Ok(())) = result {
            return Ok(());
        }
    }

    tracing::warn!(
        ?remote_mapped_addr,
        "tcp hole punch symmetric responder fan-out dial loop timeout"
    );
    Err(anyhow::anyhow!(
        "tcp hole punch symmetric responder fan-out dial loop timeout"
    ))
}

/// Cone-side spray: dials every predicted port of the symmetric responder
/// from sockets sharing the initiator's connector port, so any mapping the
/// responder owns can complete a simultaneous open. First success wins and
/// feeds the regular transport upgrade path.
async fn spray_connect_to_predicted_ports<H, AcceptedSocket>(
    host: Arc<H>,
    transport_sink: Arc<
        dyn TcpHolePunchTransportSink<
                ConnectedSocket = <H as VirtualTcpSocketFactory>::Socket,
                AcceptedSocket = AcceptedSocket,
            >,
    >,
    target_addrs: Vec<SocketAddr>,
    local_port: u16,
    context: SocketContext,
    admission: TcpHolePunchAdmission,
    scheme: &str,
) -> anyhow::Result<()>
where
    H: VirtualTcpSocketFactory,
    AcceptedSocket: 'static,
{
    tracing::info!(
        targets = target_addrs.len(),
        local_port,
        "tcp hole punch initiator start spray connect loop"
    );

    let is_v6 = target_addrs.first().is_some_and(|addr| addr.is_ipv6());
    let context = context.with_ip_version(if is_v6 { IpVersion::V6 } else { IpVersion::V4 });
    // All spray sockets share the connector port: the symmetric responder's
    // NAT only lets return traffic through for mappings created from it.
    let bind_addr = bind_addr_for_port(local_port, is_v6);

    let mut tasks = JoinSet::new();
    for target_addr in target_addrs {
        let bind = hole_punch_bind_options(context.clone(), bind_addr);
        let options = TcpConnectOptions::hole_punch(target_addr, Some(bind_addr)).with_bind(bind);
        let host = host.clone();
        let transport_sink = transport_sink.clone();
        let requested_url = mapped_addr_url(scheme, target_addr)?;
        tasks.spawn(async move {
            let start = crate::foundation::time::Instant::now();
            while start.elapsed() < SPRAY_WINDOW {
                if let Ok(Ok(socket)) = crate::foundation::time::timeout(
                    SPRAY_DIAL_TIMEOUT,
                    host.connect_tcp(options.clone()),
                )
                .await
                {
                    let admission_result = transport_sink
                        .add_connected_transport(socket, requested_url.clone(), admission)
                        .await;
                    match admission_result {
                        Ok(()) => {}
                        Err(TcpHolePunchTransportError::Upgrade(error)) => return Err(error),
                        Err(TcpHolePunchTransportError::Admission(error)) => {
                            tracing::warn!(
                                ?target_addr,
                                ?error,
                                "tcp hole punch spray tunnel admission failed"
                            );
                            continue;
                        }
                    }

                    tracing::info!(
                        ?target_addr,
                        "tcp hole punch spray connected and added tunnel"
                    );
                    return Ok(());
                }
                let sleep_ms = rand::thread_rng().gen_range(50..150);
                crate::foundation::time::sleep(Duration::from_millis(sleep_ms)).await;
            }
            Err(anyhow::anyhow!("tcp hole punch spray timeout"))
        });
    }

    while let Some(result) = tasks.join_next().await {
        if let Ok(Ok(())) = result {
            return Ok(());
        }
    }

    tracing::warn!("tcp hole punch initiator spray connect loop timeout");
    Err(anyhow::anyhow!(
        "tcp hole punch initiator spray connect loop timeout"
    ))
}

pub async fn accept_connections<L, ConnectedSocket>(
    listener: Arc<L>,
    transport_sink: Arc<
        dyn TcpHolePunchTransportSink<ConnectedSocket = ConnectedSocket, AcceptedSocket = L::Socket>,
    >,
    dst_peer_id: PeerId,
    scheme: &str,
) -> anyhow::Result<()>
where
    L: VirtualTcpListener,
    ConnectedSocket: 'static,
{
    loop {
        match listener.accept().await {
            Ok((socket, _)) => {
                let local_url = format!("{scheme}://0.0.0.0:{}", listener.local_addr()?.port())
                    .parse()
                    .map_err(|error: url::ParseError| {
                        anyhow::anyhow!("invalid local URL: {error}")
                    })?;
                if let Err(error) = transport_sink
                    .add_accepted_transport(socket, local_url)
                    .await
                {
                    tracing::error!(?error, "tcp hole punch transport admission error");
                    continue;
                }

                tracing::info!(
                    dst_peer_id,
                    "tcp hole punch initiator accepted and added server tunnel"
                );
            }
            Err(error) => {
                tracing::error!(?error, "tcp hole punch accept error");
            }
        }
    }
}

const BLACKLIST_TIMEOUT: Duration = Duration::from_secs(3600);

/// Local ports the symmetric responder fans out toward the cone initiator.
/// Each dial keeps one NAT mapping alive inside the initiator's spray window.
const SYMMETRIC_RESPONDER_SOCKET_COUNT: usize = 32;
/// Ports advertised to the cone initiator for spraying (`k = 0..max-1`).
const PREDICTED_PORT_NUM: u32 = 50;
/// Upper bound for a peer-supplied `max_port_num`, bounding spray sockets.
const PREDICTED_PORT_NUM_LIMIT: u32 = 50;
/// How long the responder keeps its fan-out sockets dialing. Must outlive an
/// unpatched initiator's connect timeout plus its fallback-listener window.
const SYMMETRIC_RESPONDER_HOLD: Duration = Duration::from_secs(15);
const SYMMETRIC_RESPONDER_DIAL_TIMEOUT: Duration = Duration::from_secs(3);
/// Overall window for the initiator-side spray, matching the accept window.
const SPRAY_WINDOW: Duration = Duration::from_secs(10);
const SPRAY_DIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Bind flags that let the punch port carry a listener and dial sockets at
/// the same time (SO_REUSEADDR everywhere, plus SO_REUSEPORT where it exists;
/// Windows tolerates the shared bind with SO_REUSEADDR alone).
fn hole_punch_bind_options(socket_context: SocketContext, bind_addr: SocketAddr) -> TcpBindOptions {
    TcpBindOptions::default()
        .with_context(socket_context)
        .with_local_addr(Some(bind_addr))
        .with_reuse_addr(true)
        .with_reuse_port(true)
        .with_only_v6(true)
}

fn fallback_listener_options(
    socket_context: SocketContext,
    bind_addr: std::net::SocketAddr,
) -> TcpListenOptions {
    let bind = hole_punch_bind_options(socket_context.with_ip_version(IpVersion::V4), bind_addr);
    TcpListenOptions::hole_punch(bind_addr).with_bind(bind)
}

struct TcpHolePunchBlacklist {
    entries: DashMap<PeerId, Instant>,
}

impl TcpHolePunchBlacklist {
    fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    fn insert(&self, peer_id: PeerId) {
        self.entries.insert(peer_id, Instant::now());
    }

    fn contains(&self, peer_id: PeerId) -> bool {
        let active = self
            .entries
            .get(&peer_id)
            .is_some_and(|inserted_at| inserted_at.elapsed() < BLACKLIST_TIMEOUT);
        if !active {
            self.entries.remove(&peer_id);
        }
        active
    }

    fn cleanup(&self) {
        self.entries
            .retain(|_, inserted_at| inserted_at.elapsed() < BLACKLIST_TIMEOUT);
    }
}

fn handle_rpc_result<T>(
    result: Result<T, rpc_types::error::Error>,
    dst_peer_id: PeerId,
    blacklist: &TcpHolePunchBlacklist,
) -> Result<T, rpc_types::error::Error> {
    match result {
        Ok(result) => Ok(result),
        Err(error) => {
            if matches!(error, rpc_types::error::Error::InvalidServiceKey(_, _)) {
                blacklist.insert(dst_peer_id);
            }
            Err(error)
        }
    }
}

fn is_symmetric_tcp_nat(nat_type: NatType) -> bool {
    matches!(
        nat_type,
        NatType::Symmetric | NatType::SymmetricEasyInc | NatType::SymmetricEasyDec
    )
}

fn is_easy_symmetric_tcp_nat(nat_type: NatType) -> bool {
    matches!(
        nat_type,
        NatType::SymmetricEasyInc | NatType::SymmetricEasyDec
    )
}

/// Whether a node with `local_nat_type` should initiate TCP punches. Unknown
/// is allowed and relies on the punch window plus backoff as the safety net.
fn initiator_eligible(local_nat_type: NatType) -> bool {
    !is_symmetric_tcp_nat(local_nat_type)
}

/// Whether the RPC responder answers with the symmetric multi-socket dialer.
///
/// Easy-symmetric responders publish a port prediction window; unknown
/// responders still fan out because the fan-out needs no remote cooperation.
/// Plain symmetric NATs keep the legacy single-port dialer.
fn responder_uses_multi_socket(local_nat_type: NatType, disable_sym_hole_punching: bool) -> bool {
    !disable_sym_hole_punching
        && (local_nat_type == NatType::Unknown || is_easy_symmetric_tcp_nat(local_nat_type))
}

/// Public ports the cone initiator should dial, starting at the responder's
/// STUN base mapping and walking in the NAT's allocation direction.
fn spray_target_addrs(
    base: SocketAddr,
    direction: PortSequenceDirection,
    max_port_num: u32,
) -> Vec<SocketAddr> {
    let count = max_port_num.clamp(1, PREDICTED_PORT_NUM_LIMIT);
    match direction {
        PortSequenceDirection::Incremental => (0..count)
            .filter_map(|offset| base.port().checked_add(offset as u16))
            .map(|port| SocketAddr::new(base.ip(), port))
            .collect(),
        PortSequenceDirection::Decremental => (0..count)
            .filter_map(|offset| base.port().checked_sub(offset as u16))
            .filter(|port| *port != 0)
            .map(|port| SocketAddr::new(base.ip(), port))
            .collect(),
        PortSequenceDirection::Unknown => Vec::new(),
    }
}

fn port_sequence_direction(nat_type: NatType) -> PortSequenceDirection {
    match nat_type {
        NatType::SymmetricEasyInc => PortSequenceDirection::Incremental,
        NatType::SymmetricEasyDec => PortSequenceDirection::Decremental,
        _ => PortSequenceDirection::Unknown,
    }
}

struct TcpHolePunchServer<H, P>
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource,
{
    host: Arc<H>,
    peer_source: Arc<P>,
    stun: Arc<dyn StunInfoProvider>,
    socket_context: SocketContext,
    transport_sink: Arc<TcpHolePunchTransportSinkFor<H>>,
    tasks: Arc<Mutex<JoinSet<()>>>,
    reaper: Mutex<Option<AbortOnDropHandle<()>>>,
    stopping: AtomicBool,
}

impl<H, P> TcpHolePunchServer<H, P>
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource,
{
    fn new(
        host: Arc<H>,
        peer_source: Arc<P>,
        stun: Arc<dyn StunInfoProvider>,
        socket_context: SocketContext,
        transport_sink: Arc<TcpHolePunchTransportSinkFor<H>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            peer_source,
            stun,
            socket_context,
            transport_sink,
            tasks: Arc::new(Mutex::new(JoinSet::new())),
            reaper: Mutex::new(None),
            stopping: AtomicBool::new(true),
        })
    }

    fn supports_wss_hole_punching(&self) -> bool {
        self.transport_sink.supports_wss_hole_punching()
    }

    /// Whether an inbound punch from `caller` is accepted under the local
    /// P2P policy. Only the `need_p2p` exception passes a disabled node;
    /// unknown callers are rejected because their intent cannot be verified.
    async fn accept_inbound_punch(&self, caller_peer_id: Option<u64>) -> bool {
        if !self.peer_source.p2p_policy_flags().disable_p2p {
            return true;
        }
        let Some(caller_peer_id) =
            caller_peer_id.and_then(|peer_id| PeerId::try_from(peer_id).ok())
        else {
            return false;
        };
        let caller_need_p2p = self
            .peer_source
            .candidates()
            .await
            .into_iter()
            .find(|candidate| candidate.peer_id == caller_peer_id)
            .and_then(|candidate| candidate.feature_flag)
            .is_some_and(|flag| flag.need_p2p);
        should_accept_inbound_punch(true, caller_need_p2p)
    }

    fn start(&self) {
        let mut reaper = self.reaper.lock().unwrap();
        if reaper.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        {
            let _tasks = self.tasks.lock().unwrap();
            self.stopping.store(false, Ordering::Release);
        }
        reaper.replace(AbortOnDropHandle::new(tokio::spawn(
            reap_joinset_background(Arc::downgrade(&self.tasks), "tcp hole punch"),
        )));
    }

    fn begin_stop(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    async fn stop(&self) {
        let reaper = self.reaper.lock().unwrap().take();
        if let Some(reaper) = reaper {
            reaper.abort();
            let _ = reaper.await;
        }
        let mut tasks = {
            let mut task_slot = self.tasks.lock().unwrap();
            std::mem::replace(&mut *task_slot, JoinSet::new())
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

#[async_trait]
impl<H, P> TcpHolePunchRpc for TcpHolePunchServer<H, P>
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource,
{
    type Controller = BaseController;

    #[tracing::instrument(skip(self), fields(a_mapped_addr = ?input.connector_mapped_addr), err)]
    async fn exchange_mapped_addr(
        &self,
        controller: Self::Controller,
        input: TcpHolePunchRequest,
    ) -> rpc_types::error::Result<TcpHolePunchResponse> {
        let local_nat_type =
            NatType::try_from(self.stun.get_stun_info().tcp_nat_type).unwrap_or(NatType::Unknown);
        let policy = self.peer_source.p2p_policy_flags();
        tracing::debug!(?local_nat_type, "tcp hole punch rpc received");
        if local_nat_type == NatType::Unknown {
            // Detection gaps should not block the punch: the initiator's
            // window and backoff bound the cost of a failed attempt.
            tracing::warn!(
                ?local_nat_type,
                "tcp hole punch rpc with unknown local nat type, continuing"
            );
        }
        if !self
            .accept_inbound_punch(controller.get_caller_peer_id())
            .await
        {
            tracing::warn!(
                caller = ?controller.get_caller_peer_id(),
                "tcp hole punch rpc rejected (P2P disabled by local policy)"
            );
            return Err(
                anyhow::anyhow!("TCP hole punching is disabled by the local P2P policy").into(),
            );
        }
        let requested_scheme = input.scheme.trim().to_owned();
        if requested_scheme.is_empty() && policy.only_use_wss_http3_for_hole_punching {
            return Err(anyhow::anyhow!(
                "raw TCP hole punching is disabled by WSS/HTTP3-only policy"
            )
            .into());
        }
        if requested_scheme == "wss" && policy.disable_wss_http3_for_p2p {
            return Err(
                anyhow::anyhow!("WSS hole punching is disabled by the local P2P policy").into(),
            );
        }
        if requested_scheme == "wss" && !self.transport_sink.supports_wss_hole_punching() {
            return Err(anyhow::anyhow!("this node does not support WSS hole punching").into());
        }
        if !matches!(requested_scheme.as_str(), "" | "wss") {
            return Err(
                anyhow::anyhow!("unsupported TCP hole-punch scheme: {requested_scheme}").into(),
            );
        }

        let remote_mapped_addr = input
            .connector_mapped_addr
            .ok_or_else(|| anyhow::anyhow!("connector_mapped_addr is required"))?;
        let remote_mapped_addr: std::net::SocketAddr = remote_mapped_addr.into();
        let remote_ip = remote_mapped_addr.ip();
        if remote_ip.is_unspecified() || remote_ip.is_multicast() {
            tracing::warn!(
                ?remote_mapped_addr,
                "tcp hole punch rpc invalid connector addr"
            );
            return Err(anyhow::anyhow!("connector_mapped_addr is malformed").into());
        }

        let local_port = select_local_port(
            self.host.as_ref(),
            self.socket_context.clone(),
            remote_mapped_addr.is_ipv6(),
        )
        .await?;
        let local_mapped_addr = self
            .stun
            .get_tcp_port_mapping(local_port)
            .await
            .context("failed to get tcp port mapping")?;

        tracing::info!(
            ?remote_mapped_addr,
            local_port,
            ?local_mapped_addr,
            "tcp hole punch rpc responding with listener mapped addr and start connecting"
        );

        let host = self.host.clone();
        let socket_context = self.socket_context.clone();
        let transport_sink = self.transport_sink.clone();
        let connect_scheme = if requested_scheme.is_empty() {
            "tcp".to_owned()
        } else {
            requested_scheme.clone()
        };
        let use_multi_socket_responder =
            responder_uses_multi_socket(local_nat_type, policy.disable_sym_hole_punching);
        let mut tasks = self.tasks.lock().unwrap();
        if self.stopping.load(Ordering::Acquire) {
            return Err(rpc_types::error::Error::Shutdown);
        }
        tasks.spawn(async move {
            if use_multi_socket_responder {
                let _ = connect_as_symmetric_responder(
                    host,
                    transport_sink,
                    remote_mapped_addr,
                    local_port,
                    socket_context,
                    TcpHolePunchAdmission::Client,
                    &connect_scheme,
                )
                .await;
            } else {
                let _ = try_connect_to_remote(
                    host,
                    transport_sink,
                    TcpHolePunchDialOptions {
                        remote_mapped_addr,
                        local_port,
                        context: socket_context,
                        admission: TcpHolePunchAdmission::Client,
                        max_attempts: 5,
                        scheme: &connect_scheme,
                    },
                )
                .await;
            }
        });

        // Prediction info only for peers that announced support; upstream
        // peers must keep receiving the original response shape.
        let predicted_ports = if use_multi_socket_responder
            && input.supports_port_prediction
            && is_easy_symmetric_tcp_nat(local_nat_type)
        {
            Some(TcpHolePunchPredictedPorts {
                base_mapped_addr: Some(local_mapped_addr.into()),
                max_port_num: PREDICTED_PORT_NUM,
                direction: port_sequence_direction(local_nat_type) as i32,
            })
        } else {
            None
        };

        Ok(TcpHolePunchResponse {
            listener_mapped_addr: Some(local_mapped_addr.into()),
            scheme: requested_scheme,
            predicted_ports,
        })
    }
}

struct TcpHolePunchConnectorData<H, P>
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource,
{
    host: Arc<H>,
    stun: Arc<dyn StunInfoProvider>,
    socket_context: SocketContext,
    peer_source: Arc<P>,
    transport_sink: Arc<TcpHolePunchTransportSinkFor<H>>,
    supports_wss_hole_punching: bool,
    blacklist: TcpHolePunchBlacklist,
}

impl<H, P> TcpHolePunchConnectorData<H, P>
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource,
{
    async fn punch_as_initiator(self: Arc<Self>, dst_peer_id: PeerId) -> anyhow::Result<()> {
        let mut backoff = BackOff::new(vec![1000, 1000, 4000, 8000]);

        loop {
            backoff.sleep_for_next_backoff().await;
            if self.do_punch_as_initiator(dst_peer_id).await.is_ok() {
                break;
            }

            if self.blacklist.contains(dst_peer_id) {
                tracing::warn!(
                    dst_peer_id,
                    "tcp hole punch initiator skipped (blacklisted)"
                );
                break;
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(dst_peer_id), err)]
    async fn do_punch_as_initiator(&self, dst_peer_id: PeerId) -> anyhow::Result<()> {
        let policy = self.peer_source.p2p_policy_flags();
        let peer_policy = self
            .peer_source
            .candidates()
            .await
            .into_iter()
            .find(|candidate| candidate.peer_id == dst_peer_id)
            .map(|candidate| candidate.peer_disguise_flags)
            .unwrap_or_default();
        let use_wss =
            policy.use_wss_http3_with_peer(&peer_policy) && self.supports_wss_hole_punching;
        let allow_raw = policy.allow_raw_with_peer(&peer_policy);
        let mut requested_scheme = if use_wss { "wss" } else { "tcp" };
        if !use_wss && !allow_raw {
            tracing::debug!(
                dst_peer_id,
                "tcp hole punch skipped by WSS/HTTP3 P2P policy"
            );
            return Ok(());
        }
        let local_nat_type =
            NatType::try_from(self.stun.get_stun_info().tcp_nat_type).unwrap_or(NatType::Unknown);
        tracing::debug!(?local_nat_type, "tcp hole punch initiator start");
        if !initiator_eligible(local_nat_type) {
            tracing::debug!("tcp hole punch initiator skipped (symmetric)");
            return Ok(());
        }

        let local_port =
            select_local_port(self.host.as_ref(), self.socket_context.clone(), false).await?;
        let local_mapped_addr = self
            .stun
            .get_tcp_port_mapping(local_port)
            .await
            .context("failed to get tcp port mapping")?;

        tracing::info!(
            dst_peer_id,
            local_port,
            ?local_mapped_addr,
            "tcp hole punch initiator got mapped addr, start rpc exchange"
        );

        let rpc_stub = self.peer_source.rpc_stub(dst_peer_id);
        let supports_port_prediction = !policy.disable_sym_hole_punching;
        let punch_request = TcpHolePunchRequest {
            connector_mapped_addr: Some(local_mapped_addr.into()),
            scheme: if requested_scheme == "tcp" {
                String::new()
            } else {
                requested_scheme.to_owned()
            },
            supports_port_prediction,
        };
        let mut response = rpc_stub
            .exchange_mapped_addr(
                BaseController {
                    timeout_ms: 6000,
                    ..Default::default()
                },
                punch_request.clone(),
            )
            .await;
        // A remote that cannot serve WSS rejects the exchange outright (e.g.
        // a feature-trimmed build that still broadcasts a preference). Retry
        // once with raw TCP when policy allows it, and stop hammering the
        // peer when no fallback is permitted.
        if requested_scheme == "wss"
            && matches!(&response, Err(rpc_types::error::Error::ExecutionError(_)))
        {
            if !allow_raw {
                self.blacklist.insert(dst_peer_id);
                anyhow::bail!("peer {dst_peer_id} rejected WSS hole punching");
            }
            tracing::warn!(
                dst_peer_id,
                "peer rejected WSS hole punching, retrying raw TCP"
            );
            requested_scheme = "tcp";
            response = self
                .peer_source
                .rpc_stub(dst_peer_id)
                .exchange_mapped_addr(
                    BaseController {
                        timeout_ms: 6000,
                        ..Default::default()
                    },
                    TcpHolePunchRequest {
                        scheme: String::new(),
                        ..punch_request
                    },
                )
                .await;
        }
        let response = handle_rpc_result(response, dst_peer_id, &self.blacklist)?;
        if requested_scheme == "wss" && response.scheme != "wss" {
            if !allow_raw {
                self.blacklist.insert(dst_peer_id);
                anyhow::bail!(
                    "peer {dst_peer_id} did not negotiate WSS hole punching; scheme={}",
                    response.scheme
                );
            }
            requested_scheme = "tcp";
        }
        let remote_mapped_addr = response
            .listener_mapped_addr
            .ok_or_else(|| anyhow::anyhow!("listener_mapped_addr is required"))?;
        let remote_mapped_addr = remote_mapped_addr.into();
        tracing::info!(
            dst_peer_id,
            ?remote_mapped_addr,
            "tcp hole punch initiator rpc returned"
        );

        // Port prediction is only trusted when sym punching is enabled; an
        // absent or unusable prediction falls back to the legacy single-port
        // simultaneous open, which also covers unpatched responders.
        let spray_addrs = response
            .predicted_ports
            .filter(|_| !policy.disable_sym_hole_punching)
            .and_then(|predicted| {
                let direction = PortSequenceDirection::try_from(predicted.direction)
                    .unwrap_or(PortSequenceDirection::Unknown);
                let base = predicted.base_mapped_addr.map(SocketAddr::from)?;
                let targets = spray_target_addrs(base, direction, predicted.max_port_num);
                (!targets.is_empty()).then_some(targets)
            });

        let bind_addr =
            std::net::SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), local_port);
        // Bind the fallback listener before dialing so SYNs arriving during
        // the dial leg find an open port instead of an RST. The dial sockets
        // share the port via the reuse flags; platforms that still refuse the
        // concurrent bind retry below once the dial socket is gone.
        let mut fallback_listener = self
            .host
            .bind_tcp(fallback_listener_options(
                self.socket_context.clone(),
                bind_addr,
            ))
            .await
            .ok();

        let dial_result = match spray_addrs {
            Some(target_addrs) => {
                spray_connect_to_predicted_ports(
                    self.host.clone(),
                    self.transport_sink.clone(),
                    target_addrs,
                    local_port,
                    self.socket_context.clone(),
                    TcpHolePunchAdmission::Server,
                    requested_scheme,
                )
                .await
            }
            None => {
                try_connect_to_remote(
                    self.host.clone(),
                    self.transport_sink.clone(),
                    TcpHolePunchDialOptions {
                        remote_mapped_addr,
                        local_port,
                        context: self.socket_context.clone(),
                        admission: TcpHolePunchAdmission::Server,
                        max_attempts: 1,
                        scheme: requested_scheme,
                    },
                )
                .await
            }
        };

        if dial_result.is_ok() {
            tracing::info!(
                dst_peer_id,
                local_port,
                ?remote_mapped_addr,
                "tcp hole punch initiator connected to remote mapped addr with simultaneous connection"
            );
            return Ok(());
        }

        tracing::debug!(
            dst_peer_id,
            local_port,
            ?remote_mapped_addr,
            "tcp hole punch initiator sent syn to remote mapped addr"
        );

        if fallback_listener.is_none() {
            fallback_listener = self
                .host
                .bind_tcp(fallback_listener_options(
                    self.socket_context.clone(),
                    bind_addr,
                ))
                .await
                .ok();
        }
        let listener = fallback_listener
            .ok_or_else(|| anyhow::anyhow!("failed to bind tcp hole punch fallback listener"))?;

        tracing::info!(
            dst_peer_id,
            local_port,
            local_addr = ?listener.local_addr()?,
            "tcp hole punch initiator listening"
        );

        crate::foundation::time::timeout(
            Duration::from_secs(10),
            accept_connections(
                listener,
                self.transport_sink.clone(),
                dst_peer_id,
                requested_scheme,
            ),
        )
        .await??;

        tracing::info!(
            dst_peer_id,
            "tcp hole punch initiator accepted and added server tunnel"
        );
        Ok(())
    }

    async fn collect_peers_need_task(&self) -> Vec<PeerId> {
        let policy = self.peer_source.p2p_policy_flags();

        let local_nat_type =
            NatType::try_from(self.stun.get_stun_info().tcp_nat_type).unwrap_or(NatType::Unknown);
        if !initiator_eligible(local_nat_type) {
            tracing::trace!(
                ?local_nat_type,
                "tcp hole punch task collect skipped (symmetric)"
            );
            return Vec::new();
        }

        self.blacklist.cleanup();
        let local_peer_id = self.peer_source.local_peer_id();
        let mut peers_to_connect = Vec::new();
        for candidate in self.peer_source.candidates().await {
            let use_wss_engine = policy.use_wss_http3_with_peer(&candidate.peer_disguise_flags)
                && self.supports_wss_hole_punching;
            let static_allowed = should_background_p2p_with_peer(
                candidate.feature_flag.as_ref(),
                false,
                policy.lazy_p2p,
                policy.disable_p2p,
                policy.need_p2p,
            );
            let dynamic_allowed = should_try_p2p_with_peer(
                candidate.feature_flag.as_ref(),
                false,
                policy.disable_p2p,
                policy.need_p2p,
            ) && (candidate.has_recent_traffic
                || (candidate.has_direct_connection
                    && use_wss_engine
                    && !candidate.has_disguised_connection));
            if !static_allowed && !dynamic_allowed {
                continue;
            }

            let allow_raw = policy.allow_raw_with_peer(&candidate.peer_disguise_flags);
            if !use_wss_engine && !allow_raw {
                continue;
            }

            let peer_id = candidate.peer_id;
            if peer_id == local_peer_id {
                tracing::trace!(peer_id, "tcp hole punch task collect skip self");
                continue;
            }
            if self.blacklist.contains(peer_id) {
                tracing::debug!(peer_id, "tcp hole punch task collect skip blacklisted");
                continue;
            }
            if candidate.has_direct_connection
                && (!use_wss_engine || candidate.has_disguised_connection)
            {
                tracing::trace!(peer_id, "tcp hole punch task collect skip already has peer");
                continue;
            }

            let peer_nat_type = candidate.tcp_nat_type;
            // Peers with unknown NAT types are still punched: the UDP engine
            // treats the same situation as cone, and the punch window plus
            // backoff bound the cost of a wrong guess.

            tracing::info!(
                peer_id,
                local_peer_id,
                ?local_nat_type,
                ?peer_nat_type,
                "tcp hole punch task collect add peer"
            );
            peers_to_connect.push(peer_id);
        }
        peers_to_connect
    }
}

struct TcpHolePunchPeerTaskLauncher<H, P>(Arc<TcpHolePunchConnectorData<H, P>>)
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource;

impl<H, P> Clone for TcpHolePunchPeerTaskLauncher<H, P>
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[async_trait]
impl<H, P> PeerTaskLauncher for TcpHolePunchPeerTaskLauncher<H, P>
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource,
{
    type CollectPeerItem = PeerId;
    type TaskRet = ();

    async fn collect_peers_need_task(&self) -> Vec<PeerId> {
        self.0.collect_peers_need_task().await
    }

    async fn launch_task(
        &self,
        dst_peer_id: PeerId,
    ) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        let data = self.0.clone();
        tokio::spawn(async move { data.punch_as_initiator(dst_peer_id).await })
    }

    fn loop_interval_ms(&self) -> u64 {
        5000
    }
}

pub struct TcpHolePunchConnector<H, P>
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource + HolePunchTunnelSink + HolePunchRpcRegistry,
{
    server: Arc<TcpHolePunchServer<H, P>>,
    client: PeerTaskManager<TcpHolePunchPeerTaskLauncher<H, P>>,
    peer_source: Arc<P>,
}

impl<H, P> TcpHolePunchConnector<H, P>
where
    H: TcpHolePunchHost,
    P: TcpHolePunchPeerSource + HolePunchTunnelSink + HolePunchRpcRegistry,
{
    pub fn new(
        peer_source: Arc<P>,
        host: Arc<H>,
        stun: Arc<dyn StunInfoProvider>,
        socket_context: SocketContext,
        client_protocol: Arc<dyn ClientProtocolUpgrader<ConnectedTcpSocket<H>>>,
        connected_server_protocol: Arc<dyn ServerProtocolUpgrader<ConnectedTcpSocket<H>>>,
        server_protocol: Arc<dyn ServerProtocolUpgrader<AcceptedTcpSocket<H>>>,
    ) -> Self {
        let transport_sink: Arc<TcpHolePunchTransportSinkFor<H>> =
            Arc::new(ProtocolTcpHolePunchTransportSink::new(
                client_protocol,
                connected_server_protocol,
                server_protocol,
                peer_source.clone(),
            ));
        let data = Arc::new(TcpHolePunchConnectorData {
            host: host.clone(),
            stun: stun.clone(),
            socket_context: socket_context.clone(),
            peer_source: peer_source.clone(),
            transport_sink: transport_sink.clone(),
            supports_wss_hole_punching: transport_sink.supports_wss_hole_punching(),
            blacklist: TcpHolePunchBlacklist::new(),
        });
        Self {
            server: TcpHolePunchServer::new(
                host,
                peer_source.clone(),
                stun,
                socket_context,
                transport_sink,
            ),
            client: PeerTaskManager::new_with_external_signal(
                TcpHolePunchPeerTaskLauncher(data),
                Some(peer_source.p2p_demand_notify()),
            ),
            peer_source,
        }
    }

    pub fn run(&self) {
        if self.peer_source.tcp_hole_punching_disabled() {
            tracing::debug!("tcp hole punch disabled by runtime configuration");
            return;
        }
        let policy = self.peer_source.p2p_policy_flags();
        if policy.only_use_wss_http3_for_hole_punching && !self.server.supports_wss_hole_punching()
        {
            tracing::debug!("WSS hole punching disabled because the runtime lacks WSS");
            return;
        }
        self.server.start();
        self.peer_source
            .register_rpc_service(TcpHolePunchRpcServer::new_arc(self.server.clone()));
        self.client.start();
    }

    pub async fn stop(&self) {
        self.client.stop().await;
        self.server.begin_stop();
        self.peer_source
            .unregister_rpc_service(TcpHolePunchRpcServer::new_arc(self.server.clone()));
        self.server.stop().await;
    }

    /// Whether the full punch path (client, connected-server and
    /// accepted-server upgraders) can serve WSS. Test-only seam for
    /// verifying the production wiring of all three protocol upgraders.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn supports_wss_hole_punching(&self) -> bool {
        self.server.supports_wss_hole_punching()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::proto::common::StunInfo;
    use crate::tunnel::Tunnel;

    #[derive(Default)]
    struct MockProtocols {
        client_upgrades: AtomicUsize,
        server_upgrades: AtomicUsize,
        fail_client_upgrade: AtomicBool,
    }

    #[async_trait]
    impl ClientProtocolUpgrader<()> for MockProtocols {
        fn supports_scheme(&self, scheme: &str) -> bool {
            matches!(scheme, "tcp" | "wss")
        }

        async fn upgrade_client(
            &self,
            connected: ConnectedTransport<()>,
            _requested_url: url::Url,
        ) -> anyhow::Result<Box<dyn Tunnel>> {
            let ConnectedTransport::Tcp(()) = connected else {
                anyhow::bail!("expected TCP transport");
            };
            if self.fail_client_upgrade.load(Ordering::Relaxed) {
                anyhow::bail!("mock client upgrade failure");
            }
            self.client_upgrades.fetch_add(1, Ordering::Relaxed);
            Ok(crate::tunnel::ring::create_ring_tunnel_pair().0)
        }
    }

    #[async_trait]
    impl ServerProtocolUpgrader<()> for MockProtocols {
        fn supports_scheme(&self, scheme: &str) -> bool {
            matches!(scheme, "tcp" | "wss")
        }

        async fn upgrade_tcp(
            &self,
            _socket: (),
            _local_url: url::Url,
        ) -> anyhow::Result<ServerProtocolUpgrade> {
            self.server_upgrades.fetch_add(1, Ordering::Relaxed);
            Ok(ServerProtocolUpgrade::Tunnel(
                crate::tunnel::ring::create_ring_tunnel_pair().0,
            ))
        }

        async fn upgrade_udp(
            &self,
            _session: crate::socket::udp::UdpSession,
            _local_url: url::Url,
            _admission: Option<crate::connectivity::protocol::ServerProtocolAdmission>,
        ) -> anyhow::Result<ServerProtocolUpgrade> {
            anyhow::bail!("unexpected UDP transport")
        }

        async fn upgrade_byte_stream(
            &self,
            _socket: (),
            _local_url: url::Url,
            _remote_url: Option<url::Url>,
        ) -> anyhow::Result<ServerProtocolUpgrade> {
            anyhow::bail!("unexpected byte stream")
        }
    }

    // Same fake upgraders for the mock host's socket type.
    #[async_trait]
    impl ClientProtocolUpgrader<MockPunchSocket> for MockProtocols {
        fn supports_scheme(&self, scheme: &str) -> bool {
            matches!(scheme, "tcp" | "wss")
        }

        async fn upgrade_client(
            &self,
            connected: ConnectedTransport<MockPunchSocket>,
            _requested_url: url::Url,
        ) -> anyhow::Result<Box<dyn Tunnel>> {
            let ConnectedTransport::Tcp(_) = connected else {
                anyhow::bail!("expected TCP transport");
            };
            if self.fail_client_upgrade.load(Ordering::Relaxed) {
                anyhow::bail!("mock client upgrade failure");
            }
            self.client_upgrades.fetch_add(1, Ordering::Relaxed);
            Ok(crate::tunnel::ring::create_ring_tunnel_pair().0)
        }
    }

    #[async_trait]
    impl ServerProtocolUpgrader<MockPunchSocket> for MockProtocols {
        fn supports_scheme(&self, scheme: &str) -> bool {
            matches!(scheme, "tcp" | "wss")
        }

        async fn upgrade_tcp(
            &self,
            _socket: MockPunchSocket,
            _local_url: url::Url,
        ) -> anyhow::Result<ServerProtocolUpgrade> {
            self.server_upgrades.fetch_add(1, Ordering::Relaxed);
            Ok(ServerProtocolUpgrade::Tunnel(
                crate::tunnel::ring::create_ring_tunnel_pair().0,
            ))
        }

        async fn upgrade_udp(
            &self,
            _session: crate::socket::udp::UdpSession,
            _local_url: url::Url,
            _admission: Option<crate::connectivity::protocol::ServerProtocolAdmission>,
        ) -> anyhow::Result<ServerProtocolUpgrade> {
            anyhow::bail!("unexpected UDP transport")
        }

        async fn upgrade_byte_stream(
            &self,
            _socket: MockPunchSocket,
            _local_url: url::Url,
            _remote_url: Option<url::Url>,
        ) -> anyhow::Result<ServerProtocolUpgrade> {
            anyhow::bail!("unexpected byte stream")
        }
    }

    #[derive(Default)]
    struct MockTunnelSink {
        clients: AtomicUsize,
        servers: AtomicUsize,
        fail_client_admission: AtomicBool,
    }

    #[async_trait]
    impl HolePunchTunnelSink for MockTunnelSink {
        async fn add_client_tunnel(&self, _tunnel: Box<dyn Tunnel>) -> anyhow::Result<()> {
            if self.fail_client_admission.load(Ordering::Relaxed) {
                anyhow::bail!("mock client admission failure");
            }
            self.clients.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn add_server_tunnel(&self, _tunnel: Box<dyn Tunnel>) -> anyhow::Result<()> {
            self.servers.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn protocol_sink_upgrades_before_tcp_hole_punch_admission() {
        let protocols = Arc::new(MockProtocols::default());
        let tunnel_sink = Arc::new(MockTunnelSink::default());
        let sink = ProtocolTcpHolePunchTransportSink::new(
            protocols.clone(),
            protocols.clone(),
            protocols.clone(),
            tunnel_sink.clone(),
        );
        let url = url::Url::parse("tcp://198.51.100.1:11010").unwrap();

        sink.add_connected_transport((), url.clone(), TcpHolePunchAdmission::Client)
            .await
            .unwrap();
        sink.add_connected_transport((), url.clone(), TcpHolePunchAdmission::Server)
            .await
            .unwrap();
        sink.add_accepted_transport((), url).await.unwrap();

        assert_eq!(protocols.client_upgrades.load(Ordering::Relaxed), 2);
        assert_eq!(protocols.server_upgrades.load(Ordering::Relaxed), 1);
        assert_eq!(tunnel_sink.clients.load(Ordering::Relaxed), 1);
        assert_eq!(tunnel_sink.servers.load(Ordering::Relaxed), 2);

        sink.add_connected_transport(
            (),
            url::Url::parse("wss://198.51.100.1:443").unwrap(),
            TcpHolePunchAdmission::Server,
        )
        .await
        .unwrap();
        assert_eq!(protocols.server_upgrades.load(Ordering::Relaxed), 2);
        assert_eq!(tunnel_sink.servers.load(Ordering::Relaxed), 3);

        protocols.fail_client_upgrade.store(true, Ordering::Relaxed);
        assert!(matches!(
            sink.add_connected_transport(
                (),
                url::Url::parse("tcp://198.51.100.1:11010").unwrap(),
                TcpHolePunchAdmission::Client,
            )
            .await,
            Err(TcpHolePunchTransportError::Upgrade(_))
        ));
        protocols
            .fail_client_upgrade
            .store(false, Ordering::Relaxed);
        tunnel_sink
            .fail_client_admission
            .store(true, Ordering::Relaxed);
        assert!(matches!(
            sink.add_connected_transport(
                (),
                url::Url::parse("tcp://198.51.100.1:11010").unwrap(),
                TcpHolePunchAdmission::Client,
            )
            .await,
            Err(TcpHolePunchTransportError::Admission(_))
        ));
    }

    #[test]
    fn bind_address_tracks_requested_family_and_port() {
        assert_eq!(
            bind_addr_for_port(1234, false),
            "0.0.0.0:1234".parse().unwrap()
        );
        assert_eq!(bind_addr_for_port(4321, true), "[::]:4321".parse().unwrap());
    }

    #[test]
    fn symmetric_tcp_nat_variants_are_ineligible_initiators() {
        assert!(is_symmetric_tcp_nat(NatType::Symmetric));
        assert!(is_symmetric_tcp_nat(NatType::SymmetricEasyInc));
        assert!(is_symmetric_tcp_nat(NatType::SymmetricEasyDec));
        assert!(!is_symmetric_tcp_nat(NatType::PortRestricted));
        assert!(!is_symmetric_tcp_nat(NatType::Unknown));
    }

    #[test]
    fn blacklist_tracks_and_cleans_entries() {
        let blacklist = TcpHolePunchBlacklist::new();
        assert!(!blacklist.contains(7));
        blacklist.insert(7);
        assert!(blacklist.contains(7));
        blacklist.cleanup();
        assert!(blacklist.contains(7));
    }

    #[test]
    fn fallback_listener_normalizes_context_to_ipv4() {
        let bind_addr = "0.0.0.0:23333".parse().unwrap();
        let context = SocketContext::default()
            .with_socket_mark(Some(0))
            .with_netns(Some(crate::socket::NetNamespace::new("test-netns")));

        let options = fallback_listener_options(context, bind_addr);

        assert_eq!(
            options.purpose,
            crate::socket::tcp::TcpListenPurpose::HolePunch
        );
        assert_eq!(options.bind.local_addr, Some(bind_addr));
        assert_eq!(options.bind.context.ip_version, IpVersion::V4);
        assert_eq!(options.bind.context.socket_mark, Some(0));
        assert_eq!(
            options
                .bind
                .context
                .netns
                .as_ref()
                .map(|netns| netns.token()),
            Some("test-netns")
        );
        assert!(options.bind.only_v6);
        // The listener shares the punch port with the dial sockets so both
        // can be alive during the simultaneous-open window.
        assert_eq!(options.bind.reuse_addr, Some(true));
        assert!(options.bind.reuse_port);
    }

    // ---- Mock hole-punch host with an ordered event log ----

    /// Socket type for the mock host: satisfies the stream contract without
    /// carrying data; reads hit EOF immediately.
    struct MockPunchSocket(SocketAddr, SocketAddr);

    impl tokio::io::AsyncRead for MockPunchSocket {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl tokio::io::AsyncWrite for MockPunchSocket {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl crate::socket::tcp::VirtualTcpSocket for MockPunchSocket {
        fn local_addr(&self) -> std::io::Result<SocketAddr> {
            Ok(self.0)
        }

        fn peer_addr(&self) -> std::io::Result<SocketAddr> {
            Ok(self.1)
        }
    }

    const MOCK_LOCAL_PORT: u16 = 12345;
    const MOCK_CONNECTOR_PORT: u16 = 55779;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MockConnectMode {
        FailFast,
        Succeed,
        Pending,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum MockHostEvent {
        BindListener {
            port: u16,
        },
        Connect {
            remote_port: u16,
            local_port: u16,
            reuse_addr: Option<bool>,
            reuse_port: bool,
        },
    }

    struct MockHolePunchHost {
        events: Mutex<Vec<MockHostEvent>>,
        connect_mode: MockConnectMode,
        accept_queue: Arc<Mutex<VecDeque<MockPunchSocket>>>,
    }

    impl MockHolePunchHost {
        fn new(connect_mode: MockConnectMode) -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
                connect_mode,
                accept_queue: Arc::new(Mutex::new(VecDeque::new())),
            })
        }

        fn record(&self, event: MockHostEvent) {
            self.events.lock().unwrap().push(event);
        }

        fn events(&self) -> Vec<MockHostEvent> {
            self.events.lock().unwrap().clone()
        }

        fn position(&self, event: &MockHostEvent) -> Option<usize> {
            self.events.lock().unwrap().iter().position(|e| e == event)
        }

        fn count_connects_with_local_port(&self, local_port: u16) -> usize {
            self.events()
                .into_iter()
                .filter(|event| {
                    matches!(
                        event,
                        MockHostEvent::Connect { local_port: port, .. } if *port == local_port
                    )
                })
                .count()
        }

        async fn wait_for_connects(&self, local_port: u16, min_count: usize) -> usize {
            let deadline = crate::foundation::time::Instant::now() + Duration::from_secs(2);
            loop {
                let count = self.count_connects_with_local_port(local_port);
                if count >= min_count || crate::foundation::time::Instant::now() > deadline {
                    return count;
                }
                crate::foundation::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    struct MockHolePunchListener {
        local_port: u16,
        accept_queue: Arc<Mutex<VecDeque<MockPunchSocket>>>,
    }

    #[async_trait]
    impl VirtualTcpListener for MockHolePunchListener {
        type Socket = MockPunchSocket;

        fn local_addr(&self) -> std::io::Result<SocketAddr> {
            Ok(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                self.local_port,
            ))
        }

        async fn accept(&self) -> std::io::Result<(Self::Socket, SocketAddr)> {
            let peer_addr: SocketAddr = "203.0.113.50:55555".parse().expect("valid addr");
            if let Some(socket) = self.accept_queue.lock().unwrap().pop_front() {
                return Ok((socket, peer_addr));
            }
            std::future::pending().await
        }
    }

    #[async_trait]
    impl VirtualTcpListenerFactory for MockHolePunchHost {
        type Listener = MockHolePunchListener;

        async fn bind_tcp(&self, options: TcpListenOptions) -> anyhow::Result<Arc<Self::Listener>> {
            let port = options.bind.local_addr.map(|addr| addr.port()).unwrap_or(0);
            self.record(MockHostEvent::BindListener { port });
            // Mimic the stack handing out a fixed ephemeral port for port 0.
            let local_port = if port == 0 { MOCK_LOCAL_PORT } else { port };
            Ok(Arc::new(MockHolePunchListener {
                local_port,
                accept_queue: self.accept_queue.clone(),
            }))
        }
    }

    #[async_trait]
    impl VirtualTcpSocketFactory for MockHolePunchHost {
        type Socket = MockPunchSocket;

        async fn connect_tcp(&self, options: TcpConnectOptions) -> anyhow::Result<Self::Socket> {
            self.record(MockHostEvent::Connect {
                remote_port: options.remote_addr.port(),
                local_port: options.bind.local_addr.map(|addr| addr.port()).unwrap_or(0),
                reuse_addr: options.bind.reuse_addr,
                reuse_port: options.bind.reuse_port,
            });
            match self.connect_mode {
                MockConnectMode::FailFast => anyhow::bail!("mock connect refused"),
                MockConnectMode::Succeed => Ok(MockPunchSocket(
                    options.bind.local_addr.unwrap_or(options.remote_addr),
                    options.remote_addr,
                )),
                MockConnectMode::Pending => std::future::pending().await,
            }
        }
    }

    struct MockStunProvider {
        tcp_nat_type: NatType,
    }

    #[async_trait]
    impl crate::connectivity::stun::StunInfoProvider for MockStunProvider {
        fn get_stun_info(&self) -> StunInfo {
            StunInfo {
                tcp_nat_type: self.tcp_nat_type as i32,
                ..Default::default()
            }
        }

        async fn get_udp_port_mapping(&self, _local_port: u16) -> anyhow::Result<SocketAddr> {
            Ok("198.51.100.20:40000".parse().expect("valid addr"))
        }

        async fn get_tcp_port_mapping(&self, _local_port: u16) -> anyhow::Result<SocketAddr> {
            Ok("198.51.100.30:41000".parse().expect("valid addr"))
        }

        fn update_stun_info(&self) {}
    }

    struct MockTcpHolePunchRpc {
        response: TcpHolePunchResponse,
        requests: Arc<Mutex<Vec<TcpHolePunchRequest>>>,
    }

    #[async_trait]
    impl TcpHolePunchRpc for MockTcpHolePunchRpc {
        type Controller = BaseController;

        async fn exchange_mapped_addr(
            &self,
            _controller: BaseController,
            input: TcpHolePunchRequest,
        ) -> rpc_types::error::Result<TcpHolePunchResponse> {
            self.requests.lock().unwrap().push(input);
            Ok(self.response.clone())
        }
    }

    struct MockPeerSource {
        policy: P2pPolicyFlags,
        rpc: Arc<MockTcpHolePunchRpc>,
    }

    #[async_trait]
    impl TcpHolePunchPeerSource for MockPeerSource {
        fn local_peer_id(&self) -> PeerId {
            1
        }

        fn p2p_policy_flags(&self) -> P2pPolicyFlags {
            self.policy
        }

        fn tcp_hole_punching_disabled(&self) -> bool {
            false
        }

        fn p2p_demand_notify(&self) -> Arc<ExternalTaskSignal> {
            Arc::new(ExternalTaskSignal::new())
        }

        async fn candidates(&self) -> Vec<TcpPunchCandidate> {
            vec![TcpPunchCandidate {
                peer_id: 2,
                tcp_nat_type: NatType::FullCone,
                feature_flag: None,
                has_direct_connection: false,
                has_disguised_connection: false,
                has_recent_traffic: true,
                peer_disguise_flags: PeerDisguiseP2pFlags::default(),
            }]
        }

        fn rpc_stub(
            &self,
            _dst_peer_id: PeerId,
        ) -> Box<dyn TcpHolePunchRpc<Controller = BaseController> + Send + Sync + 'static> {
            Box::new(MockTcpHolePunchRpc {
                response: self.rpc.response.clone(),
                requests: self.rpc.requests.clone(),
            })
        }
    }

    fn mock_transport_sink() -> (
        Arc<TcpHolePunchTransportSinkFor<MockHolePunchHost>>,
        Arc<MockTunnelSink>,
    ) {
        let protocols = Arc::new(MockProtocols::default());
        let tunnel_sink = Arc::new(MockTunnelSink::default());
        let transport_sink: Arc<TcpHolePunchTransportSinkFor<MockHolePunchHost>> =
            Arc::new(ProtocolTcpHolePunchTransportSink::new(
                protocols.clone(),
                protocols.clone(),
                protocols.clone(),
                tunnel_sink.clone(),
            ));
        (transport_sink, tunnel_sink)
    }

    fn mock_connector_data(
        host: Arc<MockHolePunchHost>,
        tcp_nat_type: NatType,
        policy: P2pPolicyFlags,
        rpc_response: TcpHolePunchResponse,
    ) -> (
        Arc<TcpHolePunchConnectorData<MockHolePunchHost, MockPeerSource>>,
        Arc<MockTcpHolePunchRpc>,
        Arc<MockTunnelSink>,
    ) {
        let protocols = Arc::new(MockProtocols::default());
        let tunnel_sink = Arc::new(MockTunnelSink::default());
        let transport_sink: Arc<TcpHolePunchTransportSinkFor<MockHolePunchHost>> =
            Arc::new(ProtocolTcpHolePunchTransportSink::new(
                protocols.clone(),
                protocols.clone(),
                protocols.clone(),
                tunnel_sink.clone(),
            ));
        let rpc = Arc::new(MockTcpHolePunchRpc {
            response: rpc_response,
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        let data = Arc::new(TcpHolePunchConnectorData {
            host,
            stun: Arc::new(MockStunProvider { tcp_nat_type }),
            socket_context: SocketContext::default(),
            peer_source: Arc::new(MockPeerSource {
                policy,
                rpc: rpc.clone(),
            }),
            transport_sink,
            supports_wss_hole_punching: false,
            blacklist: TcpHolePunchBlacklist::new(),
        });
        (data, rpc, tunnel_sink)
    }

    fn legacy_response() -> TcpHolePunchResponse {
        TcpHolePunchResponse {
            listener_mapped_addr: Some("198.51.100.99:45678".parse::<SocketAddr>().unwrap().into()),
            scheme: String::new(),
            predicted_ports: None,
        }
    }

    fn predicted_ports_response() -> TcpHolePunchResponse {
        TcpHolePunchResponse {
            listener_mapped_addr: Some("198.51.100.99:45678".parse::<SocketAddr>().unwrap().into()),
            scheme: String::new(),
            predicted_ports: Some(TcpHolePunchPredictedPorts {
                base_mapped_addr: Some("198.51.100.99:40000".parse::<SocketAddr>().unwrap().into()),
                max_port_num: 10,
                direction: PortSequenceDirection::Incremental as i32,
            }),
        }
    }

    #[test]
    fn initiator_eligibility_allows_unknown_nat() {
        assert!(initiator_eligible(NatType::Unknown));
        assert!(initiator_eligible(NatType::OpenInternet));
        assert!(initiator_eligible(NatType::FullCone));
        assert!(initiator_eligible(NatType::PortRestricted));
        assert!(!initiator_eligible(NatType::Symmetric));
        assert!(!initiator_eligible(NatType::SymmetricEasyInc));
        assert!(!initiator_eligible(NatType::SymmetricEasyDec));
    }

    #[test]
    fn responder_multi_socket_mode_matrix() {
        assert!(responder_uses_multi_socket(NatType::Unknown, false));
        assert!(responder_uses_multi_socket(
            NatType::SymmetricEasyInc,
            false
        ));
        assert!(responder_uses_multi_socket(
            NatType::SymmetricEasyDec,
            false
        ));
        // Hard symmetric keeps the legacy dialer and sym punching can be
        // switched off entirely without touching the unknown fallback.
        assert!(!responder_uses_multi_socket(NatType::Symmetric, false));
        assert!(!responder_uses_multi_socket(NatType::FullCone, false));
        assert!(!responder_uses_multi_socket(NatType::Unknown, true));
        assert!(!responder_uses_multi_socket(
            NatType::SymmetricEasyInc,
            true
        ));
    }

    #[test]
    fn port_sequence_direction_maps_nat_types() {
        assert_eq!(
            port_sequence_direction(NatType::SymmetricEasyInc),
            PortSequenceDirection::Incremental
        );
        assert_eq!(
            port_sequence_direction(NatType::SymmetricEasyDec),
            PortSequenceDirection::Decremental
        );
        assert_eq!(
            port_sequence_direction(NatType::Unknown),
            PortSequenceDirection::Unknown
        );
        assert_eq!(
            port_sequence_direction(NatType::FullCone),
            PortSequenceDirection::Unknown
        );
    }

    fn ports_of(addrs: &[SocketAddr]) -> Vec<u16> {
        addrs.iter().map(|addr| addr.port()).collect()
    }

    #[test]
    fn spray_targets_walk_the_sequence_direction() {
        let base: SocketAddr = "198.51.100.9:40000".parse().unwrap();
        assert_eq!(
            ports_of(&spray_target_addrs(
                base,
                PortSequenceDirection::Incremental,
                3
            )),
            vec![40000, 40001, 40002]
        );
        assert_eq!(
            ports_of(&spray_target_addrs(
                base,
                PortSequenceDirection::Decremental,
                3
            )),
            vec![40000, 39999, 39998]
        );
        // Unknown direction never sprays.
        assert!(spray_target_addrs(base, PortSequenceDirection::Unknown, 10).is_empty());
        // Zero or oversized windows are clamped.
        assert_eq!(
            ports_of(&spray_target_addrs(
                base,
                PortSequenceDirection::Incremental,
                0
            )),
            vec![40000]
        );
        assert_eq!(
            spray_target_addrs(base, PortSequenceDirection::Incremental, u32::MAX).len(),
            PREDICTED_PORT_NUM_LIMIT as usize
        );
        // The port space is clamped at the u16 boundaries; k = 0 is the base.
        let top: SocketAddr = "198.51.100.9:65535".parse().unwrap();
        assert_eq!(
            ports_of(&spray_target_addrs(
                top,
                PortSequenceDirection::Incremental,
                5
            )),
            vec![65535]
        );
        let bottom: SocketAddr = "198.51.100.9:1".parse().unwrap();
        assert_eq!(
            ports_of(&spray_target_addrs(
                bottom,
                PortSequenceDirection::Decremental,
                5
            )),
            vec![1]
        );
    }

    #[tokio::test]
    async fn initiator_binds_fallback_listener_before_dialing() {
        let host = MockHolePunchHost::new(MockConnectMode::FailFast);
        host.accept_queue.lock().unwrap().push_back(MockPunchSocket(
            "0.0.0.0:12345".parse().unwrap(),
            "203.0.113.50:55555".parse().unwrap(),
        ));
        let (data, _rpc, tunnel_sink) = mock_connector_data(
            host.clone(),
            NatType::PortRestricted,
            P2pPolicyFlags::default(),
            legacy_response(),
        );

        // accept_connections loops after the first accept; run it in a task
        // and only assert the observable ordering and admission.
        let punch = tokio::spawn(async move { data.do_punch_as_initiator(2).await });
        let deadline = crate::foundation::time::Instant::now() + Duration::from_secs(2);
        while tunnel_sink.servers.load(Ordering::Relaxed) == 0
            && crate::foundation::time::Instant::now() < deadline
        {
            crate::foundation::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(tunnel_sink.servers.load(Ordering::Relaxed) >= 1);

        let bind_event = MockHostEvent::BindListener {
            port: MOCK_LOCAL_PORT,
        };
        let connect_event = MockHostEvent::Connect {
            remote_port: 45678,
            local_port: MOCK_LOCAL_PORT,
            reuse_addr: Some(true),
            reuse_port: true,
        };
        let bind_index = host
            .position(&bind_event)
            .expect("fallback listener bound on the punch port");
        let connect_index = host
            .position(&connect_event)
            .expect("dial from the punch port to the remote listener");
        assert!(
            bind_index < connect_index,
            "fallback listener must exist before the dial leg starts, events: {:?}",
            host.events()
        );
        // The early bind succeeded, so no re-bind happened after the dial.
        assert_eq!(
            host.events()
                .iter()
                .filter(|event| **event == bind_event)
                .count(),
            1
        );
        punch.abort();
        let _ = punch.await;
    }

    #[tokio::test]
    async fn initiator_with_unknown_local_nat_still_punches() {
        let host = MockHolePunchHost::new(MockConnectMode::FailFast);
        host.accept_queue.lock().unwrap().push_back(MockPunchSocket(
            "0.0.0.0:12345".parse().unwrap(),
            "203.0.113.50:55555".parse().unwrap(),
        ));
        let (data, rpc, _tunnel_sink) = mock_connector_data(
            host.clone(),
            NatType::Unknown,
            P2pPolicyFlags::default(),
            legacy_response(),
        );

        let punch = tokio::spawn(async move { data.do_punch_as_initiator(2).await });
        let deadline = crate::foundation::time::Instant::now() + Duration::from_secs(2);
        while rpc.requests.lock().unwrap().is_empty()
            && crate::foundation::time::Instant::now() < deadline
        {
            crate::foundation::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !rpc.requests.lock().unwrap().is_empty(),
            "unknown local nat type must not skip the punch"
        );
        assert!(rpc.requests.lock().unwrap()[0].supports_port_prediction);
        punch.abort();
        let _ = punch.await;
    }

    #[tokio::test]
    async fn symmetric_initiator_skips_without_side_effects() {
        let host = MockHolePunchHost::new(MockConnectMode::FailFast);
        let (data, rpc, _tunnel_sink) = mock_connector_data(
            host.clone(),
            NatType::SymmetricEasyInc,
            P2pPolicyFlags::default(),
            legacy_response(),
        );

        data.do_punch_as_initiator(2).await.unwrap();

        assert!(rpc.requests.lock().unwrap().is_empty());
        assert!(host.events().is_empty());
    }

    #[tokio::test]
    async fn predicted_ports_response_switches_to_spray() {
        let host = MockHolePunchHost::new(MockConnectMode::Succeed);
        let (data, _rpc, _tunnel_sink) = mock_connector_data(
            host.clone(),
            NatType::FullCone,
            P2pPolicyFlags::default(),
            predicted_ports_response(),
        );

        data.do_punch_as_initiator(2).await.unwrap();

        let connects: Vec<MockHostEvent> = host
            .events()
            .into_iter()
            .filter(|event| matches!(event, MockHostEvent::Connect { .. }))
            .collect();
        assert!(!connects.is_empty());
        for event in &connects {
            let MockHostEvent::Connect {
                remote_port,
                local_port,
                reuse_addr,
                reuse_port,
            } = event
            else {
                unreachable!("filtered above");
            };
            // Targets walk the predicted window from the shared connector port.
            assert!((40000..40010).contains(remote_port));
            assert_eq!(*local_port, MOCK_LOCAL_PORT);
            assert_eq!(*reuse_addr, Some(true));
            assert!(reuse_port);
        }
        // The fallback listener is still bound alongside the spray.
        assert!(host.events().contains(&MockHostEvent::BindListener {
            port: MOCK_LOCAL_PORT
        }));
    }

    #[tokio::test]
    async fn predicted_ports_ignored_without_prediction_response() {
        let host = MockHolePunchHost::new(MockConnectMode::FailFast);
        host.accept_queue.lock().unwrap().push_back(MockPunchSocket(
            "0.0.0.0:12345".parse().unwrap(),
            "203.0.113.50:55555".parse().unwrap(),
        ));
        let (data, _rpc, _tunnel_sink) = mock_connector_data(
            host.clone(),
            NatType::FullCone,
            P2pPolicyFlags::default(),
            legacy_response(),
        );

        let punch = tokio::spawn(async move { data.do_punch_as_initiator(2).await });
        let deadline = crate::foundation::time::Instant::now() + Duration::from_secs(2);
        loop {
            let connects = host.count_connects_with_local_port(MOCK_LOCAL_PORT);
            if connects >= 1 || crate::foundation::time::Instant::now() > deadline {
                assert_eq!(connects, 1, "legacy path dials exactly once");
                break;
            }
            crate::foundation::time::sleep(Duration::from_millis(10)).await;
        }
        punch.abort();
        let _ = punch.await;
    }

    #[tokio::test]
    async fn disabled_sym_punching_ignores_predicted_ports() {
        let host = MockHolePunchHost::new(MockConnectMode::FailFast);
        host.accept_queue.lock().unwrap().push_back(MockPunchSocket(
            "0.0.0.0:12345".parse().unwrap(),
            "203.0.113.50:55555".parse().unwrap(),
        ));
        let (data, rpc, _tunnel_sink) = mock_connector_data(
            host.clone(),
            NatType::FullCone,
            P2pPolicyFlags {
                disable_sym_hole_punching: true,
                ..Default::default()
            },
            predicted_ports_response(),
        );

        let punch = tokio::spawn(async move { data.do_punch_as_initiator(2).await });
        let deadline = crate::foundation::time::Instant::now() + Duration::from_secs(2);
        loop {
            let connects = host.count_connects_with_local_port(MOCK_LOCAL_PORT);
            if connects >= 1 || crate::foundation::time::Instant::now() > deadline {
                assert_eq!(connects, 1, "sym-disabled punch keeps the single dial");
                break;
            }
            crate::foundation::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!rpc.requests.lock().unwrap()[0].supports_port_prediction);
        punch.abort();
        let _ = punch.await;
    }

    async fn run_responder_exchange(
        tcp_nat_type: NatType,
        policy: P2pPolicyFlags,
        request: TcpHolePunchRequest,
    ) -> (
        Arc<MockHolePunchHost>,
        Arc<TcpHolePunchServer<MockHolePunchHost, MockPeerSource>>,
        rpc_types::error::Result<TcpHolePunchResponse>,
    ) {
        let host = MockHolePunchHost::new(MockConnectMode::Pending);
        let (transport_sink, _tunnel_sink) = mock_transport_sink();
        let rpc = Arc::new(MockTcpHolePunchRpc {
            response: TcpHolePunchResponse::default(),
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        let server = TcpHolePunchServer::new(
            host.clone(),
            Arc::new(MockPeerSource {
                policy,
                rpc: rpc.clone(),
            }),
            Arc::new(MockStunProvider { tcp_nat_type }),
            SocketContext::default(),
            transport_sink,
        );
        server.start();
        let result =
            TcpHolePunchRpc::exchange_mapped_addr(&*server, BaseController::default(), request)
                .await;
        (host, server, result)
    }

    fn upstream_format_request() -> TcpHolePunchRequest {
        // Fields introduced after the original wire format stay at their
        // defaults, exactly like an unpatched peer would send them.
        TcpHolePunchRequest {
            connector_mapped_addr: Some("203.0.113.99:55779".parse::<SocketAddr>().unwrap().into()),
            scheme: String::new(),
            supports_port_prediction: false,
        }
    }

    #[tokio::test]
    async fn responder_multi_socket_dials_fan_out_from_distinct_ports() {
        let (host, server, result) = run_responder_exchange(
            NatType::SymmetricEasyInc,
            P2pPolicyFlags::default(),
            upstream_format_request(),
        )
        .await;

        let response = result.expect("easy-sym responder accepts the exchange");
        // Upstream peers must not receive prediction info.
        assert_eq!(
            response.listener_mapped_addr,
            Some("198.51.100.30:41000".parse::<SocketAddr>().unwrap().into())
        );
        assert!(response.predicted_ports.is_none());

        let connects = host
            .wait_for_connects(0, SYMMETRIC_RESPONDER_SOCKET_COUNT - 1)
            .await;
        assert_eq!(
            connects,
            SYMMETRIC_RESPONDER_SOCKET_COUNT - 1,
            "ephemeral fan-out sockets, events: {:?}",
            host.events()
        );
        // The first socket re-dials from the announced listener port and all
        // sockets target the initiator's connector address.
        assert_eq!(host.count_connects_with_local_port(MOCK_LOCAL_PORT), 1);
        for event in host.events() {
            if let MockHostEvent::Connect { remote_port, .. } = event {
                assert_eq!(remote_port, MOCK_CONNECTOR_PORT);
            }
        }
        server.stop().await;
    }

    #[tokio::test]
    async fn responder_publishes_predicted_ports_for_supporting_peers() {
        let (host, server, result) = run_responder_exchange(
            NatType::SymmetricEasyDec,
            P2pPolicyFlags::default(),
            TcpHolePunchRequest {
                supports_port_prediction: true,
                ..upstream_format_request()
            },
        )
        .await;

        let response = result.expect("easy-sym responder accepts the exchange");
        let predicted = response.predicted_ports.expect("prediction advertised");
        assert_eq!(
            predicted.base_mapped_addr,
            Some("198.51.100.30:41000".parse::<SocketAddr>().unwrap().into())
        );
        assert_eq!(predicted.max_port_num, PREDICTED_PORT_NUM);
        assert_eq!(
            PortSequenceDirection::try_from(predicted.direction),
            Ok(PortSequenceDirection::Decremental)
        );

        host.wait_for_connects(0, SYMMETRIC_RESPONDER_SOCKET_COUNT - 1)
            .await;
        server.stop().await;
    }

    #[tokio::test]
    async fn unknown_responder_continues_and_fans_out_without_prediction() {
        let (host, server, result) = run_responder_exchange(
            NatType::Unknown,
            P2pPolicyFlags::default(),
            TcpHolePunchRequest {
                supports_port_prediction: true,
                ..upstream_format_request()
            },
        )
        .await;

        let response = result.expect("unknown local nat type must not reject the exchange");
        assert!(response.predicted_ports.is_none());

        host.wait_for_connects(0, SYMMETRIC_RESPONDER_SOCKET_COUNT - 1)
            .await;
        server.stop().await;
    }

    #[tokio::test]
    async fn hard_symmetric_and_disabled_sym_responders_keep_legacy_dialer() {
        for (nat_type, policy) in [
            (NatType::Symmetric, P2pPolicyFlags::default()),
            (
                NatType::SymmetricEasyInc,
                P2pPolicyFlags {
                    disable_sym_hole_punching: true,
                    ..Default::default()
                },
            ),
            (NatType::FullCone, P2pPolicyFlags::default()),
        ] {
            let host = MockHolePunchHost::new(MockConnectMode::FailFast);
            let (transport_sink, _tunnel_sink) = mock_transport_sink();
            let rpc = Arc::new(MockTcpHolePunchRpc {
                response: TcpHolePunchResponse::default(),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let server = TcpHolePunchServer::new(
                host.clone(),
                Arc::new(MockPeerSource { policy, rpc }),
                Arc::new(MockStunProvider {
                    tcp_nat_type: nat_type,
                }),
                SocketContext::default(),
                transport_sink,
            );
            server.start();
            let result = TcpHolePunchRpc::exchange_mapped_addr(
                &*server,
                BaseController::default(),
                TcpHolePunchRequest {
                    supports_port_prediction: true,
                    ..upstream_format_request()
                },
            )
            .await;
            let response = result.expect("exchange accepted");
            assert!(
                response.predicted_ports.is_none(),
                "no prediction outside multi-socket mode"
            );

            // The legacy dialer retries the single announced port only.
            let connects = host.wait_for_connects(MOCK_LOCAL_PORT, 5).await;
            assert_eq!(connects, 5, "legacy dialer: five attempts on one port");
            assert_eq!(host.count_connects_with_local_port(0), 0);
            server.stop().await;
        }
    }
}
