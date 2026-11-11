#![cfg_attr(not(feature = "tcp-hole-punch"), allow(dead_code))]

use std::{
    future::Future,
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
use rand::Rng as _;
use tokio::task::JoinSet;
use tokio_util::task::AbortOnDropHandle;

use crate::{
    config::{P2pDisguiseMode, P2pPolicyFlags, PeerId},
    connectivity::{
        hole_punch::{
            HolePunchRpcRegistry, HolePunchTunnelSink, PEER_BLACKLIST_TIMEOUT,
            policy::{BackOff, accept_inbound_punch, p2p_engine_gate},
        },
        protocol::{ClientProtocolUpgrader, ServerProtocolUpgrade, ServerProtocolUpgrader},
        stun::StunInfoProvider,
        transport::ConnectedTransport,
    },
    foundation::{
        expiring_set::ExpiringSet,
        task::{ExternalTaskSignal, PeerTaskLauncher, PeerTaskManager, reap_joinset_background},
    },
    peers::PeerConnectionOrigin,
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
    pub tcp_satisfied: bool,
    pub wss_satisfied: bool,
    pub has_recent_traffic: bool,
    pub peer_disguise: P2pDisguiseMode,
}

async fn find_candidate<P: TcpHolePunchPeerSource>(
    peer_source: &P,
    peer_id: PeerId,
) -> Option<TcpPunchCandidate> {
    peer_source
        .candidates()
        .await
        .into_iter()
        .find(|candidate| candidate.peer_id == peer_id)
}

/// Narrow peer-graph view required by the TCP hole-punch engine.
///
/// Implemented only by the sealed peer adapter in `super::peer_adapters`.
#[async_trait]
pub trait TcpHolePunchPeerSource: Send + Sync + 'static {
    fn local_peer_id(&self) -> PeerId;

    fn p2p_policy_flags(&self) -> P2pPolicyFlags;
    fn reserve_parallel_slot_for_protocol(
        &self,
        _peer_id: PeerId,
        _protocol: &str,
    ) -> Option<crate::peers::conn::parallel::ParallelDialReservation> {
        None
    }
    fn parallel_connection_target(&self, _peer_id: PeerId) -> u32 {
        1
    }

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
                .add_server_tunnel(tunnel, PeerConnectionOrigin::TcpHolePunch)
                .await
                .map_err(TcpHolePunchTransportError::Admission);
        }
        let tunnel = self
            .client_protocol
            .upgrade_client(ConnectedTransport::Tcp(socket), requested_url)
            .await
            .map_err(TcpHolePunchTransportError::Upgrade)?;
        match admission {
            TcpHolePunchAdmission::Client => {
                self.tunnel_sink
                    .add_client_tunnel(tunnel, PeerConnectionOrigin::TcpHolePunch)
                    .await
            }
            TcpHolePunchAdmission::Server => {
                self.tunnel_sink
                    .add_server_tunnel(tunnel, PeerConnectionOrigin::TcpHolePunch)
                    .await
            }
        }
        .map_err(TcpHolePunchTransportError::Admission)
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
            .add_server_tunnel(tunnel, PeerConnectionOrigin::TcpHolePunch)
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

// TCP supports simultaneous connect, so both peers may dial from the mapped port.
#[allow(clippy::too_many_arguments)]
pub async fn try_connect_to_remote<H, AcceptedSocket>(
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
    max_attempts: u32,
    scheme: &str,
) -> anyhow::Result<()>
where
    H: VirtualTcpSocketFactory,
    AcceptedSocket: 'static,
{
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

async fn complete_punch_within<F>(window: Duration, attempt: F) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<()>>,
{
    // The window includes protocol upgrade and admission, not just TCP dialing.
    crate::foundation::time::timeout(window, attempt)
        .await
        .context("TCP hole-punch connect and upgrade window elapsed")?
}

/// What distinguishes one multi-socket dial from another.
#[derive(Clone, Copy)]
struct FanOutDial {
    window: Duration,
    dial_timeout: Duration,
    label: &'static str,
}

/// Redials every `(connect options, requested URL)` pair from its own task
/// until one is connected and admitted or the window elapses. The first
/// success wins; the other sockets are dropped with the task set.
async fn fan_out_dial<H, AcceptedSocket>(
    host: Arc<H>,
    transport_sink: Arc<
        dyn TcpHolePunchTransportSink<
                ConnectedSocket = <H as VirtualTcpSocketFactory>::Socket,
                AcceptedSocket = AcceptedSocket,
            >,
    >,
    dials: Vec<(TcpConnectOptions, url::Url)>,
    admission: TcpHolePunchAdmission,
    fan_out: FanOutDial,
) -> anyhow::Result<()>
where
    H: VirtualTcpSocketFactory,
    AcceptedSocket: 'static,
{
    let FanOutDial {
        window,
        dial_timeout,
        label,
    } = fan_out;
    let mut tasks = JoinSet::new();
    for (options, requested_url) in dials {
        let host = host.clone();
        let transport_sink = transport_sink.clone();
        tasks.spawn(crate::peers::conn::parallel::inherit_scope(
            complete_punch_within(window, async move {
                let start = crate::foundation::time::Instant::now();
                while start.elapsed() < window {
                    match crate::foundation::time::timeout(
                        dial_timeout,
                        host.connect_tcp(options.clone()),
                    )
                    .await
                    {
                        Ok(Ok(socket)) => match transport_sink
                            .add_connected_transport(socket, requested_url.clone(), admission)
                            .await
                        {
                            Ok(()) => {
                                tracing::info!(
                                    %requested_url,
                                    label,
                                    "tcp hole punch connected and added tunnel"
                                );
                                return Ok(());
                            }
                            Err(TcpHolePunchTransportError::Upgrade(error)) => return Err(error),
                            Err(TcpHolePunchTransportError::Admission(error)) => tracing::warn!(
                                %requested_url,
                                label,
                                ?error,
                                "tcp hole punch tunnel admission failed"
                            ),
                        },
                        _ => tracing::trace!(
                            %requested_url,
                            label,
                            "tcp hole punch connect attempt failed"
                        ),
                    }
                    let sleep_ms = rand::thread_rng().gen_range(50..150);
                    crate::foundation::time::sleep(Duration::from_millis(sleep_ms)).await;
                }
                Err(anyhow::anyhow!("tcp hole punch {label} timeout"))
            }),
        ));
    }

    while let Some(result) = tasks.join_next().await {
        if let Ok(Ok(())) = result {
            return Ok(());
        }
    }

    tracing::warn!(label, "tcp hole punch dial loop timeout");
    Err(anyhow::anyhow!("tcp hole punch {label} timeout"))
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
    let dials = (0..SYMMETRIC_RESPONDER_SOCKET_COUNT)
        .map(|index| {
            // The first socket reuses the announced listener port; the rest let
            // the OS pick distinct local ports, each allocating a fresh mapping.
            let port = if index == 0 { local_port } else { 0 };
            let bind_addr = bind_addr_for_port(port, is_v6);
            let bind = hole_punch_bind_options(context.clone(), bind_addr);
            (
                TcpConnectOptions::hole_punch(remote_mapped_addr, Some(bind_addr)).with_bind(bind),
                requested_url.clone(),
            )
        })
        .collect();
    fan_out_dial(
        host,
        transport_sink,
        dials,
        admission,
        FanOutDial {
            window: SYMMETRIC_RESPONDER_HOLD,
            dial_timeout: SYMMETRIC_RESPONDER_DIAL_TIMEOUT,
            label: "symmetric responder fan-out",
        },
    )
    .await
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
    let dials = target_addrs
        .into_iter()
        .map(|target_addr| {
            let bind = hole_punch_bind_options(context.clone(), bind_addr);
            Ok((
                TcpConnectOptions::hole_punch(target_addr, Some(bind_addr)).with_bind(bind),
                mapped_addr_url(scheme, target_addr)?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    fan_out_dial(
        host,
        transport_sink,
        dials,
        admission,
        FanOutDial {
            window: SPRAY_WINDOW,
            dial_timeout: SPRAY_DIAL_TIMEOUT,
            label: "predicted-port spray",
        },
    )
    .await
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

const WSS_EXCHANGE_FAILURE_BLACKLIST_THRESHOLD: u32 = 3;

/// Bounds one single-socket connect loop including its protocol upgrade;
/// equal to the loop's own dialing window.
const CONNECT_LOOP_WINDOW: Duration = Duration::from_secs(10);
/// Local ports the symmetric responder fans out toward the cone initiator.
/// Each dial keeps one NAT mapping alive inside the initiator's spray window.
const SYMMETRIC_RESPONDER_SOCKET_COUNT: usize = 32;
/// Inbound punch RPCs the responder serves concurrently. Each fan-out task
/// can hold up to `SYMMETRIC_RESPONDER_SOCKET_COUNT` sockets for
/// `SYMMETRIC_RESPONDER_HOLD`, so this bounds the worst-case handle usage;
/// requests beyond it are rejected as busy (the initiator retries later).
const MAX_CONCURRENT_RESPONDER_PUNCHES: usize = 4;
/// Ports advertised to the cone initiator for spraying (`k = 0..max-1`), and
/// the upper bound for a peer-supplied `max_port_num`, bounding spray sockets.
const PREDICTED_PORT_NUM: u32 = 50;
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
    entries: ExpiringSet<PeerId>,
    wss_exchange_failures: DashMap<PeerId, u32>,
}

impl TcpHolePunchBlacklist {
    fn new() -> Self {
        Self {
            entries: ExpiringSet::default(),
            wss_exchange_failures: DashMap::new(),
        }
    }

    fn insert(&self, peer_id: PeerId) {
        self.entries.insert(peer_id, PEER_BLACKLIST_TIMEOUT);
    }

    fn contains(&self, peer_id: PeerId) -> bool {
        self.entries.contains(&peer_id)
    }

    fn cleanup(&self) {
        self.entries.cleanup();
    }

    fn record_wss_exchange_failure(&self, peer_id: PeerId) {
        let mut failures = self.wss_exchange_failures.entry(peer_id).or_insert(0);
        *failures += 1;
        let should_blacklist = *failures >= WSS_EXCHANGE_FAILURE_BLACKLIST_THRESHOLD;
        drop(failures);
        if should_blacklist {
            self.wss_exchange_failures.remove(&peer_id);
            self.insert(peer_id);
        }
    }

    fn clear_wss_exchange_failures(&self, peer_id: PeerId) {
        self.wss_exchange_failures.remove(&peer_id);
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

/// Local TCP NAT type for punch decisions. A UDP-derived approximation
/// (`tcp_nat_type_is_approximated`, i.e. the TCP STUN test was inconclusive)
/// must not drive initiator eligibility, responder fan-out mode or spray
/// port prediction - those would all act on a guess. Treat it as `Unknown`
/// so only verified facts decide.
fn normalize_local_tcp_nat_type(stun: &dyn StunInfoProvider) -> NatType {
    let nat_type = NatType::try_from(stun.get_stun_info().tcp_nat_type).unwrap_or(NatType::Unknown);
    if nat_type != NatType::Unknown && stun.tcp_nat_type_is_approximated() {
        tracing::debug!(
            ?nat_type,
            "tcp nat type is a udp-derived approximation, treating as unknown"
        );
        return NatType::Unknown;
    }
    nat_type
}

/// Whether the RPC responder answers with the symmetric multi-socket dialer.
///
/// Easy-symmetric responders publish a port prediction window; unknown
/// responders still fan out because the fan-out needs no remote cooperation.
/// Plain symmetric NATs keep the single-port dialer.
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
    let count = max_port_num.clamp(1, PREDICTED_PORT_NUM);
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
    /// Bounds concurrent inbound punch fan-outs. Without it every accepted
    /// punch RPC spawns unconditionally, and a fan-out responder alone can
    /// hold up to `SYMMETRIC_RESPONDER_SOCKET_COUNT` sockets for
    /// `SYMMETRIC_RESPONDER_HOLD`, so a misbehaving peer could exhaust the
    /// machine's handles. Mirrors the busy gate the UDP punch server has.
    punch_permits: Arc<tokio::sync::Semaphore>,
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
            punch_permits: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_RESPONDER_PUNCHES,
            )),
        })
    }

    fn supports_wss_hole_punching(&self) -> bool {
        self.transport_sink.supports_wss_hole_punching()
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
        let local_nat_type = normalize_local_tcp_nat_type(self.stun.as_ref());
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
        if !accept_inbound_punch(
            policy.disable_p2p,
            controller.get_caller_peer_id(),
            |caller| async move {
                find_candidate(self.peer_source.as_ref(), caller)
                    .await
                    .and_then(|candidate| candidate.feature_flag)
            },
        )
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
        let parallel_member = crate::peers::conn::parallel::ParallelMemberToken::from_features(
            std::slice::from_ref(&input.parallel_member),
        );
        if requested_scheme.is_empty() && policy.disguise.requires() {
            return Err(anyhow::anyhow!(
                "raw TCP hole punching is disabled by WSS/HTTP3-only policy"
            )
            .into());
        }
        if requested_scheme == "wss" && policy.disguise.forbids() {
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
        let permit = self
            .punch_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                tracing::warn!(
                    ?remote_mapped_addr,
                    "tcp hole punch rpc rejected: responder busy with other punches"
                );
                anyhow::anyhow!("TCP hole punch responder is busy with other punches")
            })?;
        let mut tasks = self.tasks.lock().unwrap();
        if self.stopping.load(Ordering::Acquire) {
            return Err(rpc_types::error::Error::Shutdown);
        }
        tasks.spawn(crate::peers::conn::parallel::scope_optional(
            parallel_member,
            async move {
                // Freed when the punch finishes, bounding concurrent fan-outs.
                let _permit = permit;
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
                    let _ = complete_punch_within(
                        CONNECT_LOOP_WINDOW,
                        try_connect_to_remote(
                            host,
                            transport_sink,
                            remote_mapped_addr,
                            local_port,
                            socket_context,
                            TcpHolePunchAdmission::Client,
                            5,
                            &connect_scheme,
                        ),
                    )
                    .await;
                }
            },
        ));

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
        let peer_policy = find_candidate(self.peer_source.as_ref(), dst_peer_id)
            .await
            .map(|candidate| candidate.peer_disguise);
        let transport_policy = policy.transport_policy(peer_policy);
        let Some(use_wss) = transport_policy.select_disguised(self.supports_wss_hole_punching)
        else {
            tracing::debug!(
                dst_peer_id,
                "tcp hole punch skipped by WSS/HTTP3 P2P policy"
            );
            return Ok(());
        };
        let allow_raw = transport_policy.allow_raw();
        let mut requested_scheme = if use_wss { "wss" } else { "tcp" };
        let local_nat_type = normalize_local_tcp_nat_type(self.stun.as_ref());
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
            parallel_member: crate::peers::conn::parallel::DIAL_MEMBER
                .try_with(|token| token.feature())
                .unwrap_or_default(),
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
        // once with raw TCP when policy allows it. Generic execution errors
        // can also be transient (e.g. remote STUN failures), so strict mode
        // tracks consecutive failures and applies the TTL blacklist after a
        // bounded number of attempts.
        if requested_scheme == "wss"
            && allow_raw
            && matches!(&response, Err(rpc_types::error::Error::ExecutionError(_)))
        {
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
        if requested_scheme == "wss"
            && !allow_raw
            && matches!(&response, Err(rpc_types::error::Error::ExecutionError(_)))
        {
            self.blacklist.record_wss_exchange_failure(dst_peer_id);
        }
        let response = handle_rpc_result(response, dst_peer_id, &self.blacklist)?;
        self.blacklist.clear_wss_exchange_failures(dst_peer_id);
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
                complete_punch_within(
                    CONNECT_LOOP_WINDOW,
                    try_connect_to_remote(
                        self.host.clone(),
                        self.transport_sink.clone(),
                        remote_mapped_addr,
                        local_port,
                        self.socket_context.clone(),
                        TcpHolePunchAdmission::Server,
                        1,
                        requested_scheme,
                    ),
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

        let local_nat_type = normalize_local_tcp_nat_type(self.stun.as_ref());
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
            let transport_policy = policy.transport_policy(Some(candidate.peer_disguise));
            let Some(use_wss_engine) =
                transport_policy.select_disguised(self.supports_wss_hole_punching)
            else {
                continue;
            };
            let already_connected = if use_wss_engine {
                candidate.wss_satisfied
            } else {
                candidate.tcp_satisfied
            };
            if !p2p_engine_gate(
                candidate.feature_flag.as_ref(),
                false,
                &policy,
                candidate.has_recent_traffic,
                candidate.has_direct_connection,
                already_connected,
            ) {
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
            if already_connected {
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

    fn task_kind(&self) -> &'static str {
        "tcp hole punching"
    }

    async fn collect_peers_need_task(&self) -> Vec<PeerId> {
        self.0.collect_peers_need_task().await
    }

    async fn launch_task(
        &self,
        dst_peer_id: PeerId,
    ) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        let data = self.0.clone();
        tokio::spawn(async move {
            let peer_policy = find_candidate(data.peer_source.as_ref(), dst_peer_id)
                .await
                .map(|candidate| candidate.peer_disguise);
            let policy = data
                .peer_source
                .p2p_policy_flags()
                .transport_policy(peer_policy);
            let scheme = if policy
                .select_disguised(data.supports_wss_hole_punching)
                .unwrap_or(false)
            {
                "wss"
            } else {
                "tcp"
            };
            crate::peers::conn::parallel::dial_in_slot(
                data.peer_source
                    .reserve_parallel_slot_for_protocol(dst_peer_id, scheme),
                data.peer_source.parallel_connection_target(dst_peer_id),
                data.punch_as_initiator(dst_peer_id),
            )
            .await
            .unwrap_or(Ok(()))
        })
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
        if policy.disguise.requires() && !self.server.supports_wss_hole_punching() {
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::tunnel::Tunnel;

    mod custom_tests;

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

    #[derive(Default)]
    struct MockTunnelSink {
        clients: AtomicUsize,
        servers: AtomicUsize,
        fail_client_admission: AtomicBool,
    }

    #[async_trait]
    impl HolePunchTunnelSink for MockTunnelSink {
        async fn add_client_tunnel(
            &self,
            _tunnel: Box<dyn Tunnel>,
            origin: PeerConnectionOrigin,
        ) -> anyhow::Result<()> {
            assert_eq!(origin, PeerConnectionOrigin::TcpHolePunch);
            if self.fail_client_admission.load(Ordering::Relaxed) {
                anyhow::bail!("mock client admission failure");
            }
            self.clients.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn add_server_tunnel(
            &self,
            _tunnel: Box<dyn Tunnel>,
            origin: PeerConnectionOrigin,
        ) -> anyhow::Result<()> {
            assert_eq!(origin, PeerConnectionOrigin::TcpHolePunch);
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
}
