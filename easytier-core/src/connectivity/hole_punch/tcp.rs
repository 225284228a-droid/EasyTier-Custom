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
    config::{P2pPolicyFlags, PeerDisguiseP2pFlags, PeerId},
    connectivity::{
        hole_punch::{
            HolePunchRpcRegistry, HolePunchTunnelSink, PEER_BLACKLIST_TIMEOUT,
            policy::{BackOff, p2p_engine_gate, should_accept_inbound_punch},
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

pub struct TcpHolePunchDialOptions<'a> {
    pub remote_mapped_addr: SocketAddr,
    pub local_port: u16,
    pub context: SocketContext,
    pub admission: TcpHolePunchAdmission,
    pub max_attempts: u32,
    pub scheme: &'a str,
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

/// One TCP punch dial attempt: connect within a timeout and hand the socket
/// to the transport sink. Shared by the plain connect loop, the symmetric
/// responder fan-out and the predicted-port spray, which keep their own
/// loop shells, windows and sleep ranges.
struct TcpPunchDialer<'a, H, AcceptedSocket>
where
    H: VirtualTcpSocketFactory,
    AcceptedSocket: 'static,
{
    host: &'a H,
    sink: &'a dyn TcpHolePunchTransportSink<
        ConnectedSocket = <H as VirtualTcpSocketFactory>::Socket,
        AcceptedSocket = AcceptedSocket,
    >,
    admission: TcpHolePunchAdmission,
    requested_url: &'a url::Url,
}

impl<H, AcceptedSocket> TcpPunchDialer<'_, H, AcceptedSocket>
where
    H: VirtualTcpSocketFactory,
    AcceptedSocket: 'static,
{
    /// `Ok(true)`: connected and admitted, the caller can stop. `Ok(false)`:
    /// connect or admission failed (logged), retry after sleeping. `Err`:
    /// the protocol upgrade failed irrecoverably.
    async fn attempt(
        &self,
        options: &TcpConnectOptions,
        dial_timeout: Duration,
        label: &'static str,
        remote_addr: SocketAddr,
    ) -> anyhow::Result<bool> {
        let Ok(Ok(socket)) =
            crate::foundation::time::timeout(dial_timeout, self.host.connect_tcp(options.clone()))
                .await
        else {
            tracing::trace!(?remote_addr, label, "tcp hole punch connect attempt failed");
            return Ok(false);
        };
        match self
            .sink
            .add_connected_transport(socket, self.requested_url.clone(), self.admission)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    ?remote_addr,
                    label,
                    "tcp hole punch connected and added tunnel"
                );
                Ok(true)
            }
            Err(TcpHolePunchTransportError::Upgrade(error)) => Err(error),
            Err(TcpHolePunchTransportError::Admission(error)) => {
                tracing::warn!(
                    ?remote_addr,
                    label,
                    ?error,
                    "tcp hole punch tunnel admission failed"
                );
                Ok(false)
            }
        }
    }
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

    complete_punch_within(Duration::from_secs(10), async {
        let start = crate::foundation::time::Instant::now();
        let mut attempts = 0_u32;
        let dialer = TcpPunchDialer {
            host: host.as_ref(),
            sink: transport_sink.as_ref(),
            admission,
            requested_url: &requested_url,
        };
        while start.elapsed() < Duration::from_secs(10) && attempts < max_attempts {
            attempts = attempts.wrapping_add(1);
            let bind = hole_punch_bind_options(context.clone(), bind_addr);
            let options =
                TcpConnectOptions::hole_punch(remote_mapped_addr, Some(bind_addr)).with_bind(bind);
            match dialer
                .attempt(
                    &options,
                    Duration::from_secs(3),
                    "server connect loop",
                    remote_mapped_addr,
                )
                .await
            {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => return Err(error),
            }
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
    })
    .await
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
            complete_punch_within(SYMMETRIC_RESPONDER_HOLD, async {
                let start = crate::foundation::time::Instant::now();
                let dialer = TcpPunchDialer {
                    host: host.as_ref(),
                    sink: transport_sink.as_ref(),
                    admission,
                    requested_url: &requested_url,
                };
                while start.elapsed() < SYMMETRIC_RESPONDER_HOLD {
                    match dialer
                        .attempt(
                            &options,
                            SYMMETRIC_RESPONDER_DIAL_TIMEOUT,
                            "symmetric responder fan-out",
                            remote_mapped_addr,
                        )
                        .await
                    {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(error) => return Err(error),
                    }
                    let sleep_ms = rand::thread_rng().gen_range(50..150);
                    crate::foundation::time::sleep(Duration::from_millis(sleep_ms)).await;
                }
                Err(anyhow::anyhow!(
                    "tcp hole punch symmetric responder dial loop timeout"
                ))
            })
            .await
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
            complete_punch_within(SPRAY_WINDOW, async {
                let start = crate::foundation::time::Instant::now();
                let dialer = TcpPunchDialer {
                    host: host.as_ref(),
                    sink: transport_sink.as_ref(),
                    admission,
                    requested_url: &requested_url,
                };
                while start.elapsed() < SPRAY_WINDOW {
                    match dialer
                        .attempt(
                            &options,
                            SPRAY_DIAL_TIMEOUT,
                            "predicted-port spray",
                            target_addr,
                        )
                        .await
                    {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(error) => return Err(error),
                    }
                    let sleep_ms = rand::thread_rng().gen_range(50..150);
                    crate::foundation::time::sleep(Duration::from_millis(sleep_ms)).await;
                }
                Err(anyhow::anyhow!("tcp hole punch spray timeout"))
            })
            .await
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

const WSS_EXCHANGE_FAILURE_BLACKLIST_THRESHOLD: u32 = 3;

/// Local ports the symmetric responder fans out toward the cone initiator.
/// Each dial keeps one NAT mapping alive inside the initiator's spray window.
const SYMMETRIC_RESPONDER_SOCKET_COUNT: usize = 32;
/// Inbound punch RPCs the responder serves concurrently. Each fan-out task
/// can hold up to `SYMMETRIC_RESPONDER_SOCKET_COUNT` sockets for
/// `SYMMETRIC_RESPONDER_HOLD`, so this bounds the worst-case handle usage;
/// requests beyond it are rejected as busy (the initiator retries later).
const MAX_CONCURRENT_RESPONDER_PUNCHES: usize = 4;
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

/// Whether the RPC responder answers with the symmetric multi-socket dialer.
///
/// Easy-symmetric responders publish a port prediction window; unknown
/// responders still fan out because the fan-out needs no remote cooperation.
/// Plain symmetric NATs keep the legacy single-port dialer.
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
        tasks.spawn(async move {
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
            let use_wss_engine = policy.use_wss_http3_with_peer(&candidate.peer_disguise_flags)
                && self.supports_wss_hole_punching;
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
mod tests;
