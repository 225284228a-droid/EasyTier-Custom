use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use quanta::Instant;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use url::{Host, Url};

use crate::{
    config::{PeerDisguiseP2pFlags, PeerId, p2p_protocol_rank, preferred_disguised_scheme},
    connectivity::hole_punch::policy::p2p_engine_gate,
    connectivity::is_public_ipv6_candidate,
    connectivity::port_in_use_by_local_listener,
    connectivity::stun::{StunInfoProvider, StunSocketMapper},
    connectivity::{
        LocalListenerUrls, NoLocalListeners,
        protocol::{ClientProtocolUpgrader, protocol_uses_udp},
        transport::{self, ConnectedTransport, IpTransport, UdpSessionMode},
    },
    foundation::backoff::BackOff,
    foundation::expiring_set::ExpiringSet,
    foundation::task::{PeerTaskLauncher, PeerTaskManager},
    host::dns::DnsResolver,
    peers::{
        PeerConnectionOrigin, conn::peer_conn::PeerConnId,
        foreign_network::ForeignNetworkRpcRegistrar, peer_manager::PeerManagerCore,
        peer_rpc::PeerRpcManager,
    },
    process_runtime::ProtectedTcpPortRegistry,
    proto::{
        common::Void,
        peer_rpc::{
            DirectConnectorRpc, DirectConnectorRpcClientFactory,
            DirectConnectorRpcServer as GeneratedDirectConnectorRpcServer, GetIpListRequest,
            GetIpListResponse, SendUdpHolePunchPacketRequest,
        },
        rpc_types::{self, controller::BaseController},
    },
    socket::{
        IpVersion, SocketContext,
        tcp::{TcpBindOptions, TcpSocketPurpose},
        udp::{
            PreferredIpv6Source, UdpBindOptions, VirtualUdpSocket, VirtualUdpSocketFactory,
            send_v4_hole_punch_control_packet, send_v6_hole_punch_control_packet,
        },
    },
    tunnel::Tunnel,
};

use super::manual::{
    ManualConnectorHost, collect_bind_addrs, convert_idn_to_ascii, resolve_remote_addr,
    resolve_url_addrs,
};

mod udp;

const DIRECT_CONNECTOR_BLACKLIST_TIMEOUT: Duration = Duration::from_secs(300);
const INVALID_SERVICE_BLACKLIST_TIMEOUT: Duration = Duration::from_secs(3600);
const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const DIRECT_TASK_LOOP_INTERVAL_MS: u64 = 5000;
const MAX_IPV6_HOLE_PUNCH_CONNECTOR_ADDRS: usize = 16;
const MAX_UDP_HOLE_PUNCH_CONNECTOR_ADDRS: usize = 16;

#[async_trait]
pub trait DirectConnectorHost: ManualConnectorHost {
    async fn collect_ip_addrs(&self, context: &SocketContext) -> GetIpListResponse;

    async fn collect_foreign_ip_addrs(&self, context: &SocketContext) -> GetIpListResponse {
        self.collect_ip_addrs(context).await
    }

    fn mapped_listeners(&self) -> Vec<Url>;

    fn is_local_ip(&self, ip: &IpAddr) -> bool;

    async fn preferred_ipv6_source(
        &self,
        ip: Ipv6Addr,
        context: SocketContext,
    ) -> Option<PreferredIpv6Source>;

    async fn preferred_foreign_ipv6_source(
        &self,
        ip: Ipv6Addr,
        context: SocketContext,
    ) -> Option<PreferredIpv6Source> {
        self.preferred_ipv6_source(ip, context).await
    }
}

/// The direct dialer only dials IP transports; ring/unix byte streams and
/// unknown schemes are rejected at classification time.
fn direct_transport_from_url(url: &Url) -> anyhow::Result<IpTransport> {
    IpTransport::from_url(url, TcpSocketPurpose::DirectConnect)
        .filter(|transport| !matches!(transport, IpTransport::ByteStream))
        .ok_or_else(|| anyhow::anyhow!("unsupported direct transport scheme: {}", url.scheme()))
}

fn ordered_direct_listeners(
    mut listeners: Vec<Url>,
    default_protocol: &str,
    use_disguise: bool,
) -> impl Iterator<Item = Url> {
    // Consume the ascending ranks from the front so the preferred protocol
    // is attempted before any fallback.
    listeners.sort_by_key(|listener| {
        p2p_protocol_rank(default_protocol, use_disguise, listener.scheme())
    });
    let mut seen = HashSet::new();
    listeners.retain(|listener| seen.insert(listener.clone()));
    listeners.into_iter()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectConnectorOptions {
    pub default_protocol: String,
    pub enable_ipv6: bool,
    pub allow_public_server: bool,
    pub bind_device: bool,
    pub allow_interface_bind: bool,
    pub tcp_bind: TcpBindOptions,
    pub udp_bind: UdpBindOptions,
    #[serde(skip)]
    pub testing: bool,
}

impl Default for DirectConnectorOptions {
    fn default() -> Self {
        Self {
            default_protocol: crate::config::DEFAULT_PROTOCOL.to_owned(),
            enable_ipv6: true,
            allow_public_server: false,
            bind_device: false,
            allow_interface_bind: true,
            tcp_bind: TcpBindOptions::default(),
            udp_bind: UdpBindOptions::direct_connect(),
            testing: false,
        }
    }
}

impl DirectConnectorOptions {
    fn socket_context(&self, transport: IpTransport, ip_version: IpVersion) -> SocketContext {
        let context = match transport {
            IpTransport::Tcp(_) => self.tcp_bind.context.clone(),
            IpTransport::Udp(_) => self.udp_bind.context.clone(),
            IpTransport::ByteStream => SocketContext::default(),
        };
        context.with_ip_version(ip_version)
    }
}

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
struct ListenerBlacklistKey(PeerId, String);

struct DirectConnectorData<H>
where
    H: DirectConnectorHost,
{
    peer_manager: Arc<PeerManagerCore>,
    host: Arc<H>,
    protected_tcp_ports: Arc<ProtectedTcpPortRegistry>,
    stun: Arc<dyn StunSocketMapper<<H as VirtualUdpSocketFactory>::Socket>>,
    running_listeners: Arc<dyn LocalListenerUrls>,
    dns: Arc<dyn DnsResolver>,
    protocol:
        Arc<dyn ClientProtocolUpgrader<<H as crate::socket::tcp::VirtualTcpSocketFactory>::Socket>>,
    options: DirectConnectorOptions,
    listener_blacklist: ExpiringSet<ListenerBlacklistKey>,
    peer_blacklist: ExpiringSet<PeerId>,
}

impl<H> std::fmt::Debug for DirectConnectorData<H>
where
    H: DirectConnectorHost,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectConnectorData")
            .field("peer_id", &self.peer_manager.my_peer_id())
            .field("network_name", &self.peer_manager.network_name())
            .finish()
    }
}

pub(crate) struct DirectConnectorManager<H>
where
    H: DirectConnectorHost,
{
    data: Arc<DirectConnectorData<H>>,
    client: PeerTaskManager<DirectConnectorLauncher<H>>,
}

impl<H> DirectConnectorManager<H>
where
    H: DirectConnectorHost,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_running_listeners(
        peer_manager: Arc<PeerManagerCore>,
        host: Arc<H>,
        protected_tcp_ports: Arc<ProtectedTcpPortRegistry>,
        stun: Arc<dyn StunSocketMapper<<H as VirtualUdpSocketFactory>::Socket>>,
        running_listeners: Arc<dyn LocalListenerUrls>,
        dns: Arc<dyn DnsResolver>,
        protocol: Arc<
            dyn ClientProtocolUpgrader<<H as crate::socket::tcp::VirtualTcpSocketFactory>::Socket>,
        >,
        options: DirectConnectorOptions,
    ) -> Self {
        let data = Arc::new(DirectConnectorData {
            peer_manager: peer_manager.clone(),
            host,
            protected_tcp_ports,
            stun,
            running_listeners,
            dns,
            protocol,
            options,
            listener_blacklist: ExpiringSet::default(),
            peer_blacklist: ExpiringSet::default(),
        });
        let client = PeerTaskManager::new_with_external_signal(
            DirectConnectorLauncher(data.clone()),
            Some(peer_manager.p2p_demand_notify()),
        );
        Self { data, client }
    }

    pub fn run(&self) {
        self.run_as_server();
        self.run_as_client();
    }

    pub fn run_as_server(&self) {
        self.data
            .peer_manager
            .get_peer_rpc_mgr()
            .rpc_server()
            .registry()
            .register(
                GeneratedDirectConnectorRpcServer::new(
                    DirectConnectorRpcHandler::new_with_running_listeners_and_stun(
                        self.data.host.clone(),
                        Some(Arc::downgrade(&self.data.peer_manager)),
                        self.data.running_listeners.clone(),
                        self.data.options.udp_bind.context.clone(),
                        Some(self.data.stun.clone()),
                    ),
                ),
                self.data.peer_manager.network_name(),
            );
    }

    pub fn run_as_client(&self) {
        self.client.start();
    }

    pub async fn stop(&self) {
        self.client.stop().await;
        self.data
            .peer_manager
            .get_peer_rpc_mgr()
            .rpc_server()
            .registry()
            .unregister(
                GeneratedDirectConnectorRpcServer::new(
                    DirectConnectorRpcHandler::new_with_running_listeners_and_stun(
                        self.data.host.clone(),
                        Some(Arc::downgrade(&self.data.peer_manager)),
                        self.data.running_listeners.clone(),
                        self.data.options.udp_bind.context.clone(),
                        Some(self.data.stun.clone()),
                    ),
                ),
                self.data.peer_manager.network_name(),
            );
    }

    pub(crate) async fn local_address_observations_with_stun(
        &self,
        stun_info: &crate::proto::common::StunInfo,
    ) -> GetIpListResponse {
        collect_address_observations(
            self.data.host.as_ref(),
            Some(self.data.peer_manager.as_ref()),
            &self.data.options.udp_bind.context,
            false,
            Some(stun_info),
        )
        .await
    }
}

struct DirectConnectorLauncher<H>(Arc<DirectConnectorData<H>>)
where
    H: DirectConnectorHost;

/// A peer is collected for a direct-connect attempt unless it is ourselves,
/// blacklisted, already satisfies the negotiated transport policy, or is
/// excluded by the shared P2P engine gate.
fn should_collect_direct_peer(
    is_local_peer: bool,
    blacklisted: bool,
    connection_satisfies_policy: bool,
    p2p_allowed: bool,
) -> bool {
    !is_local_peer && !blacklisted && !connection_satisfies_policy && p2p_allowed
}

impl<H> Clone for DirectConnectorLauncher<H>
where
    H: DirectConnectorHost,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[async_trait]
impl<H> PeerTaskLauncher for DirectConnectorLauncher<H>
where
    H: DirectConnectorHost,
{
    type CollectPeerItem = PeerId;
    type TaskRet = ();

    fn task_kind(&self) -> &'static str {
        "direct connect"
    }

    async fn collect_peers_need_task(&self) -> Vec<PeerId> {
        let data = &self.0;
        data.peer_blacklist.cleanup();
        let my_peer_id = data.peer_manager.my_peer_id();
        let policy = data.peer_manager.p2p_policy_flags();
        let now = Instant::now();
        data.peer_manager
            .get_route()
            .list_routes()
            .await
            .into_iter()
            .filter(|route| {
                let peer_flags: Option<PeerDisguiseP2pFlags> =
                    route.feature_flag.as_ref().map(Into::into);
                let satisfied =
                    data.connection_satisfies_policy(route.peer_id, peer_flags.as_ref());
                should_collect_direct_peer(
                    route.peer_id == my_peer_id,
                    data.peer_blacklist.contains(&route.peer_id),
                    satisfied,
                    p2p_engine_gate(
                        route.feature_flag.as_ref(),
                        data.options.allow_public_server,
                        &policy,
                        data.peer_manager.has_recent_traffic(route.peer_id, now),
                        data.peer_manager.has_directly_connected_conn(route.peer_id),
                        satisfied,
                    ),
                )
            })
            .map(|route| route.peer_id)
            .collect()
    }

    async fn launch_task(&self, peer_id: PeerId) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        let data = self.0.clone();
        tokio::spawn(async move { data.try_direct_connect(peer_id).await })
    }

    fn loop_interval_ms(&self) -> u64 {
        DIRECT_TASK_LOOP_INTERVAL_MS
    }
}

impl<H> DirectConnectorData<H>
where
    H: DirectConnectorHost,
{
    fn connection_satisfies_policy(
        &self,
        peer_id: PeerId,
        peer: Option<&PeerDisguiseP2pFlags>,
    ) -> bool {
        let use_disguise = peer.map(|peer| {
            self.peer_manager
                .p2p_policy_flags()
                .use_wss_http3_with_peer(peer)
        });
        // Without route metadata keep today's dialing behavior: do not start
        // new disguised attempts. The `Option` handed to the query keeps the
        // negotiated preference untouched.
        let preferred = preferred_disguised_scheme(&self.options.default_protocol);
        let target = if use_disguise.unwrap_or(false) {
            preferred
                .filter(|scheme| self.protocol.supports_scheme(scheme))
                .or_else(|| {
                    ["wss", "http3"]
                        .into_iter()
                        .find(|scheme| self.protocol.supports_scheme(scheme))
                })
                .unwrap_or(&self.options.default_protocol)
        } else {
            &self.options.default_protocol
        };
        self.peer_manager
            .has_connection_at_least_as_preferred(peer_id, target, use_disguise)
    }
    /// Disguise flags of the destination peer, or `None` when the route (or
    /// its feature flags) is not known yet. `None` callers must treat the
    /// negotiated disguise preference as unknown instead of "disabled".
    async fn peer_disguise_flags_opt(&self, dst_peer_id: PeerId) -> Option<PeerDisguiseP2pFlags> {
        self.peer_manager
            .list_route_snapshots()
            .await
            .into_iter()
            .find(|route| route.peer_id == dst_peer_id)
            .and_then(|route| route.feature_flag.as_ref().map(Into::into))
    }

    async fn try_direct_connect(self: Arc<Self>, dst_peer_id: PeerId) -> anyhow::Result<()> {
        let mut backoff = BackOff::new(vec![1000, 2000, 2000, 5000, 5000, 10000, 30000, 60000]);
        let mut attempt = 0usize;

        loop {
            if self.peer_blacklist.contains(&dst_peer_id) {
                anyhow::bail!("peer {dst_peer_id} is blacklisted");
            }
            if attempt > 0 {
                backoff.sleep_for_next_backoff().await;
            }
            attempt += 1;

            let rpc_stub = self
                .peer_manager
                .get_peer_rpc_mgr()
                .rpc_client()
                .scoped_client::<DirectConnectorRpcClientFactory<BaseController>>(
                self.peer_manager.my_peer_id(),
                dst_peer_id,
                self.peer_manager.network_name().to_owned(),
            );
            let ip_list = match rpc_stub
                .get_ip_list(BaseController::default(), GetIpListRequest {})
                .await
            {
                Ok(ip_list) => ip_list,
                Err(error @ rpc_types::error::Error::InvalidServiceKey(_, _)) => {
                    self.peer_blacklist
                        .insert(dst_peer_id, INVALID_SERVICE_BLACKLIST_TIMEOUT);
                    return Err(error.into());
                }
                Err(error) => return Err(error.into()),
            };

            tracing::info!(?ip_list, dst_peer_id, "got direct-connect IP list");
            let result = self
                .try_direct_connect_with_ip_list(dst_peer_id, ip_list)
                .await;
            tracing::info!(?result, dst_peer_id, "direct-connect attempt returned");
            if self.connection_satisfies_policy(
                dst_peer_id,
                self.peer_disguise_flags_opt(dst_peer_id).await.as_ref(),
            ) {
                return Ok(());
            }
        }
    }

    async fn try_direct_connect_with_ip_list(
        self: &Arc<Self>,
        dst_peer_id: PeerId,
        ip_list: GetIpListResponse,
    ) -> anyhow::Result<()> {
        let p2p_policy = self.peer_manager.p2p_policy_flags();
        let peer_policy_opt = self.peer_disguise_flags_opt(dst_peer_id).await;
        let peer_policy = peer_policy_opt.unwrap_or_default();
        // Negotiated disguise preference for this dial round, frozen from one
        // route scan: `Some` when route metadata is present, `None` otherwise.
        // Per-URL tasks and punch decisions reuse this value instead of
        // re-scanning routes on every attempt; the group-level policy check
        // below still refreshes it between scheme groups.
        let use_disguise_opt = peer_policy_opt
            .as_ref()
            .map(|peer| p2p_policy.use_wss_http3_with_peer(peer));
        // Derive the listener filter from the same policy predicates the
        // tunnel upgrade path uses, so a disable flag vetoes disguised
        // listeners here exactly as it does during protocol negotiation
        // (`use_wss_http3_with_peer` / `allow_raw_with_peer`).
        let use_disguise_protocols = p2p_policy.use_wss_http3_with_peer(&peer_policy);
        let allow_raw_protocols = p2p_policy.allow_raw_with_peer(&peer_policy);
        let only_disguised_protocols = use_disguise_protocols && !allow_raw_protocols;
        if !use_disguise_protocols && !allow_raw_protocols {
            tracing::debug!(
                dst_peer_id,
                "direct connect skipped: WSS/HTTP3 policies leave no permitted protocol"
            );
            return Ok(());
        }
        let available_listeners = ip_list
            .listeners
            .clone()
            .into_iter()
            .chain(
                ip_list
                    .udp_http3_listeners
                    .iter()
                    .filter(|listener| {
                        use_disguise_protocols
                            && self.protocol.supports_scheme("http3")
                            && Url::try_from(*listener).is_ok_and(|url| url.scheme() == "http3")
                    })
                    .cloned(),
            )
            .map(Into::<Url>::into)
            .filter(|listener| listener.scheme() != "ring")
            .filter(|listener| self.protocol.supports_scheme(listener.scheme()))
            .filter(|listener| {
                if only_disguised_protocols {
                    matches!(listener.scheme(), "wss" | "http3")
                } else if !use_disguise_protocols {
                    !matches!(listener.scheme(), "wss" | "http3")
                } else {
                    true
                }
            })
            .filter(|listener| {
                mapped_listener_port(listener).is_some() && listener.host().is_some()
            })
            .filter(|listener| {
                self.options.enable_ipv6 || !matches!(listener.host(), Some(Host::Ipv6(_)))
            })
            .collect::<Vec<_>>();

        if available_listeners.is_empty() {
            anyhow::bail!("peer {dst_peer_id} has no valid listener");
        }

        let mut available_listeners = ordered_direct_listeners(
            available_listeners,
            &self.options.default_protocol,
            use_disguise_protocols,
        )
        .peekable();
        while let Some(listener) = available_listeners.peek() {
            let mut tasks = JoinSet::new();
            let current_scheme = listener.scheme().to_owned();
            while available_listeners
                .peek()
                .is_some_and(|listener| listener.scheme() == current_scheme)
            {
                let listener = available_listeners.next().expect("listener should exist");
                self.spawn_direct_connect_tasks(
                    dst_peer_id,
                    &ip_list,
                    &listener,
                    use_disguise_opt,
                    use_disguise_protocols,
                    &mut tasks,
                )
                .await;
            }
            let _ = tasks.join_all().await;
            if self.connection_satisfies_policy(dst_peer_id, peer_policy_opt.as_ref()) {
                return Ok(());
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_direct_connect_tasks(
        self: &Arc<Self>,
        dst_peer_id: PeerId,
        ip_list: &GetIpListResponse,
        listener: &Url,
        use_disguise_opt: Option<bool>,
        use_disguise_protocols: bool,
        tasks: &mut JoinSet<anyhow::Result<()>>,
    ) {
        let Ok(mut addrs) = resolve_mapped_listener_addrs(
            listener,
            self.options.tcp_bind.context.clone(),
            self.dns.as_ref(),
        )
        .await
        else {
            tracing::error!(?listener, "failed to resolve direct listener");
            return;
        };
        let listener_host = addrs.pop();
        let is_udp = protocol_uses_udp(listener.scheme());
        let local_listeners = self.running_listeners.local_listener_urls();
        let should_deny_target = |target: &SocketAddr| {
            let port_is_protected =
                port_in_use_by_local_listener(&local_listeners, target.port(), is_udp)
                    || (!is_udp && self.protected_tcp_ports.contains(target.port()));
            port_is_protected && self.host.is_local_ip(&target.ip())
        };

        let Some(listener_addr) = listener_host else {
            return;
        };
        let target_ips: Vec<IpAddr> = match listener_addr.ip() {
            // An unspecified listener host means "dial me on any of my
            // addresses": expand to the peer's observed interface and public
            // addresses (public IPv6 additionally filtered for usability).
            ip if ip.is_unspecified() => match ip {
                IpAddr::V4(_) => ip_list
                    .interface_ipv4s
                    .iter()
                    .chain(ip_list.public_ipv4.iter())
                    .map(|ip| IpAddr::V4(ip.addr.into()))
                    .collect(),
                IpAddr::V6(_) => {
                    let mut candidates = HashSet::new();
                    for ip in ip_list
                        .interface_ipv6s
                        .iter()
                        .chain(ip_list.public_ipv6.iter())
                        .map(|ip| Ipv6Addr::from(*ip))
                    {
                        if self.is_usable_public_ipv6(&ip).await {
                            candidates.insert(ip);
                        }
                    }
                    candidates.into_iter().map(IpAddr::V6).collect()
                }
            },
            IpAddr::V6(ip) if self.peer_manager.is_easytier_managed_ipv6(&ip).await => {
                tracing::debug!(?listener, "skip managed IPv6 direct target");
                return;
            }
            ip if ip.is_loopback() && !self.options.testing => Vec::new(),
            ip => vec![ip],
        };

        for ip in target_ips {
            let target = SocketAddr::new(ip, listener_addr.port());
            if should_deny_target(&target) {
                continue;
            }
            let mut url = listener.clone();
            if url.set_ip_host(ip).is_ok() {
                tasks.spawn(Self::try_connect_to_url(
                    self.clone(),
                    dst_peer_id,
                    url.to_string(),
                    use_disguise_opt,
                    use_disguise_protocols,
                ));
            }
        }
    }

    async fn try_connect_to_url(
        self: Arc<Self>,
        dst_peer_id: PeerId,
        url: String,
        use_disguise: Option<bool>,
        use_disguise_protocols: bool,
    ) -> anyhow::Result<()> {
        self.listener_blacklist.cleanup();
        let key = ListenerBlacklistKey(dst_peer_id, url.clone());
        if self.listener_blacklist.contains(&key) {
            anyhow::bail!("direct listener URL is blacklisted");
        }

        let backoffs = [1000, 2000, 4000];
        let mut backoff = BackOff::new(backoffs.to_vec());
        for attempt in 0..=backoffs.len() {
            let target_url = Url::parse(&url)?;
            let connected = || {
                self.peer_manager.has_connection_at_least_as_preferred(
                    dst_peer_id,
                    target_url.scheme(),
                    use_disguise,
                )
            };
            if connected() {
                return Ok(());
            }
            let result = self
                .connect_to_url_once(dst_peer_id, &url, use_disguise_protocols)
                .await;
            if result.is_ok() || connected() {
                return Ok(());
            }
            if attempt == backoffs.len() {
                self.listener_blacklist
                    .insert(key, DIRECT_CONNECTOR_BLACKLIST_TIMEOUT);
                return result;
            }

            let delay_ms = backoff.next_backoff_jittered();
            crate::foundation::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        unreachable!("direct URL retry loop must return")
    }

    async fn connect_to_url_once(
        &self,
        dst_peer_id: PeerId,
        raw_url: &str,
        use_disguise_protocols: bool,
    ) -> anyhow::Result<()> {
        let url = Url::parse(raw_url)?;
        // The same round-level predicate that admitted disguised listeners
        // into the candidate list decides whether http3 targets take the UDP
        // punch path, so a URL never punches against the policy that
        // selected it.
        let use_udp_hole_punch =
            url.scheme() == "udp" || (use_disguise_protocols && url.scheme() == "http3");
        let (peer_id, conn_id) = if use_udp_hole_punch {
            match url.host() {
                Some(Host::Ipv6(_)) => self.connect_public_ipv6(dst_peer_id, &url).await?,
                Some(Host::Ipv4(ip)) if is_public_ipv4(ip) => {
                    match self.connect_public_ipv4(dst_peer_id, &url).await {
                        Ok(result) => result,
                        Err(error) => {
                            tracing::debug!(?error, %url, "public IPv4 UDP punch failed; fallback");
                            self.connect_ordinary(dst_peer_id, url.clone()).await?
                        }
                    }
                }
                _ => self.connect_ordinary(dst_peer_id, url.clone()).await?,
            }
        } else {
            self.connect_ordinary(dst_peer_id, url.clone()).await?
        };

        if peer_id != dst_peer_id && !self.options.testing {
            self.peer_manager.close_peer_conn(peer_id, &conn_id).await?;
            anyhow::bail!("direct peer mismatch for {url}: expected {dst_peer_id}, got {peer_id}");
        }
        Ok(())
    }

    async fn connect_ordinary(
        &self,
        dst_peer_id: PeerId,
        url: Url,
    ) -> anyhow::Result<(PeerId, PeerConnId)> {
        let transport = direct_transport_from_url(&url)?;
        let normalized = convert_idn_to_ascii(url.clone())?;
        let default_port = mapped_listener_port(&normalized)
            .ok_or_else(|| anyhow::anyhow!("listener has no port: {url}"))?;
        let remote_addr = resolve_remote_addr(
            self.peer_manager.as_ref(),
            self.host.as_ref(),
            self.dns.as_ref(),
            &normalized,
            default_port,
            self.options.socket_context(transport, IpVersion::Both),
        )
        .await?;
        let bind_addrs = if self.options.bind_device
            && self.options.allow_interface_bind
            && transport.supports_interface_bind()
            && !remote_addr.ip().is_loopback()
        {
            collect_bind_addrs(
                self.peer_manager.as_ref(),
                self.host.as_ref(),
                transport.is_udp(),
                remote_addr,
            )
            .await?
        } else {
            Vec::new()
        };
        crate::foundation::time::timeout(DIRECT_CONNECT_TIMEOUT, async {
            let connected = match transport {
                IpTransport::Tcp(purpose) => ConnectedTransport::Tcp(
                    transport::connect_tcp(
                        self.host.clone(),
                        remote_addr,
                        bind_addrs,
                        self.options.tcp_bind.clone(),
                        purpose,
                    )
                    .await?,
                ),
                IpTransport::Udp(mode @ UdpSessionMode::Classified(_)) => {
                    let protocol = self.protocol.clone();
                    let tunnel = transport::connect_udp_with(
                        self.host.clone(),
                        remote_addr,
                        bind_addrs,
                        self.options.udp_bind.clone(),
                        mode,
                        move |session| {
                            let protocol = protocol.clone();
                            let url = url.clone();
                            async move {
                                protocol
                                    .upgrade_client(ConnectedTransport::Udp(session), url)
                                    .await
                            }
                        },
                    )
                    .await?;
                    return self.admit(tunnel, dst_peer_id).await;
                }
                IpTransport::Udp(mode) => ConnectedTransport::Udp(
                    transport::connect_udp(
                        self.host.clone(),
                        remote_addr,
                        bind_addrs,
                        self.options.udp_bind.clone(),
                        mode,
                    )
                    .await?,
                ),
                // direct_transport_from_url already rejected byte streams;
                // this arm only satisfies the shared enum's exhaustiveness.
                IpTransport::ByteStream => {
                    anyhow::bail!("unsupported direct transport scheme: {}", url.scheme())
                }
            };
            let tunnel = self.protocol.upgrade_client(connected, url).await?;
            self.admit(tunnel, dst_peer_id).await
        })
        .await?
    }

    async fn connect_public_ipv4(
        &self,
        dst_peer_id: PeerId,
        url: &Url,
    ) -> anyhow::Result<(PeerId, PeerConnId)> {
        let socket = self
            .host
            .bind_udp(
                UdpBindOptions::direct_connect()
                    .with_context(
                        self.options
                            .udp_bind
                            .context
                            .clone()
                            .with_ip_version(IpVersion::V4),
                    )
                    .with_local_addr(Some("0.0.0.0:0".parse().unwrap())),
            )
            .await?;
        let connector_addr = self
            .stun
            .get_udp_port_mapping_with_socket(socket.clone())
            .await?;
        let _ = self
            .remote_send_udp_hole_punch_packet(dst_peer_id, vec![connector_addr], None, url)
            .await;
        let remote_addr = resolve_literal_url(url, IpVersion::V4)?;
        let connected =
            udp::connect_with_socket(self.host.clone(), socket, remote_addr, url).await?;
        let tunnel = self
            .protocol
            .upgrade_client(ConnectedTransport::Udp(connected), url.clone())
            .await?;
        self.admit(tunnel, dst_peer_id).await
    }

    async fn connect_public_ipv6(
        &self,
        dst_peer_id: PeerId,
        url: &Url,
    ) -> anyhow::Result<(PeerId, PeerConnId)> {
        let socket = self
            .host
            .bind_udp(
                UdpBindOptions::direct_connect()
                    .with_context(
                        self.options
                            .udp_bind
                            .context
                            .clone()
                            .with_ip_version(IpVersion::V6),
                    )
                    .with_local_addr(Some("[::]:0".parse().unwrap())),
            )
            .await?;
        let connector_ips = self.collect_ipv6_hole_punch_candidates().await?;
        if !connector_ips.is_empty() {
            let port = socket.local_addr()?.port();
            let connector_addrs = connector_ips
                .into_iter()
                .map(|ip| SocketAddr::new(IpAddr::V6(ip), port))
                .collect();
            let preferred_src = match url.host() {
                Some(Host::Ipv6(ip)) => Some(ip),
                _ => None,
            };
            let _ = self
                .remote_send_udp_hole_punch_packet(dst_peer_id, connector_addrs, preferred_src, url)
                .await;
        }
        let remote_addr = resolve_literal_url(url, IpVersion::V6)?;
        let connected =
            udp::connect_with_socket(self.host.clone(), socket, remote_addr, url).await?;
        let tunnel = self
            .protocol
            .upgrade_client(ConnectedTransport::Udp(connected), url.clone())
            .await?;
        self.admit(tunnel, dst_peer_id).await
    }

    async fn collect_ipv6_hole_punch_candidates(&self) -> anyhow::Result<Vec<Ipv6Addr>> {
        let mut candidates = Vec::new();
        for ip in self
            .stun
            .get_stun_info()
            .public_ip
            .into_iter()
            .filter_map(|ip| ip.parse().ok())
        {
            if let IpAddr::V6(ip) = ip {
                self.push_ipv6_candidate(&mut candidates, ip).await;
            }
        }
        let interface_addrs = self.host.interface_addrs().await?;
        for ip in interface_addrs
            .interface_ipv6s
            .into_iter()
            .chain(interface_addrs.public_ipv6)
        {
            self.push_ipv6_candidate(&mut candidates, ip).await;
        }
        Ok(candidates)
    }

    async fn push_ipv6_candidate(&self, candidates: &mut Vec<Ipv6Addr>, ip: Ipv6Addr) {
        if candidates.len() < MAX_IPV6_HOLE_PUNCH_CONNECTOR_ADDRS
            && self.is_usable_public_ipv6(&ip).await
            && !candidates.contains(&ip)
        {
            candidates.push(ip);
        }
    }

    async fn is_usable_public_ipv6(&self, ip: &Ipv6Addr) -> bool {
        !self.peer_manager.is_easytier_managed_ipv6(ip).await
            && (self.options.testing || is_public_ipv6_candidate(*ip))
    }

    async fn remote_send_udp_hole_punch_packet(
        &self,
        dst_peer_id: PeerId,
        connector_addrs: Vec<SocketAddr>,
        preferred_src_ipv6: Option<Ipv6Addr>,
        remote_url: &Url,
    ) -> anyhow::Result<()> {
        if !protocol_uses_udp(remote_url.scheme()) {
            anyhow::bail!("UDP punch request requires a UDP listener: {remote_url}");
        }
        let listener_port = mapped_listener_port(remote_url)
            .ok_or_else(|| anyhow::anyhow!("listener has no port: {remote_url}"))?;
        let rpc_stub = self
            .peer_manager
            .get_peer_rpc_mgr()
            .rpc_client()
            .scoped_client::<DirectConnectorRpcClientFactory<BaseController>>(
            self.peer_manager.my_peer_id(),
            dst_peer_id,
            self.peer_manager.network_name().to_owned(),
        );
        rpc_stub
            .send_udp_hole_punch_packet(
                BaseController::default(),
                SendUdpHolePunchPacketRequest {
                    connector_addr: connector_addrs.first().copied().map(Into::into),
                    listener_port: listener_port as u32,
                    preferred_src_ipv6: preferred_src_ipv6.map(Into::into),
                    connector_addrs: connector_addrs.into_iter().map(Into::into).collect(),
                },
            )
            .await
            .with_context(|| format!("send UDP punch request to peer {dst_peer_id}"))?;
        Ok(())
    }

    async fn admit(
        &self,
        tunnel: Box<dyn Tunnel>,
        dst_peer_id: PeerId,
    ) -> anyhow::Result<(PeerId, PeerConnId)> {
        self.peer_manager
            .add_client_tunnel_with_peer_id_hint(
                tunnel,
                PeerConnectionOrigin::Direct,
                Some(dst_peer_id),
            )
            .await
            .map_err(Into::into)
    }
}

pub(crate) struct DirectConnectorRpcHandler<H>
where
    H: DirectConnectorHost,
{
    host: Arc<H>,
    peer_manager: Option<Weak<PeerManagerCore>>,
    running_listeners: Arc<dyn LocalListenerUrls>,
    socket_context: SocketContext,
    foreign_network: bool,
    stun: Option<Arc<dyn StunInfoProvider>>,
}

impl<H> Clone for DirectConnectorRpcHandler<H>
where
    H: DirectConnectorHost,
{
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            peer_manager: self.peer_manager.clone(),
            running_listeners: self.running_listeners.clone(),
            socket_context: self.socket_context.clone(),
            foreign_network: self.foreign_network,
            stun: self.stun.clone(),
        }
    }
}

impl<H> DirectConnectorRpcHandler<H>
where
    H: DirectConnectorHost,
{
    pub fn new_for_foreign_network_with_stun(
        host: Arc<H>,
        socket_context: SocketContext,
        stun: Option<Arc<dyn StunInfoProvider>>,
    ) -> Self {
        let running_listeners = Arc::new(NoLocalListeners);
        Self {
            host,
            peer_manager: None,
            running_listeners,
            socket_context,
            foreign_network: true,
            stun,
        }
    }

    fn new_with_running_listeners_and_stun(
        host: Arc<H>,
        peer_manager: Option<Weak<PeerManagerCore>>,
        running_listeners: Arc<dyn LocalListenerUrls>,
        socket_context: SocketContext,
        stun: Option<Arc<dyn StunInfoProvider>>,
    ) -> Self {
        Self {
            host,
            peer_manager,
            running_listeners,
            socket_context,
            foreign_network: false,
            stun,
        }
    }
}

async fn collect_address_observations<H>(
    host: &H,
    peer_manager: Option<&PeerManagerCore>,
    socket_context: &SocketContext,
    foreign_network: bool,
    stun_info: Option<&crate::proto::common::StunInfo>,
) -> GetIpListResponse
where
    H: DirectConnectorHost,
{
    let mut response = if foreign_network {
        host.collect_foreign_ip_addrs(socket_context).await
    } else {
        host.collect_ip_addrs(socket_context).await
    };
    if let Some(stun_info) = stun_info {
        for public_ip in &stun_info.public_ip {
            match public_ip.parse::<IpAddr>() {
                Ok(IpAddr::V4(ip)) => response.public_ipv4 = Some(ip.into()),
                Ok(IpAddr::V6(ip)) => response.public_ipv6 = Some(ip.into()),
                Err(_) => {}
            }
        }
    }
    if let Some(peer_manager) = peer_manager.filter(|_| !foreign_network) {
        let mut interface_ipv6s = Vec::with_capacity(response.interface_ipv6s.len());
        for ip in response.interface_ipv6s {
            if !peer_manager
                .is_easytier_managed_ipv6(&Ipv6Addr::from(ip))
                .await
            {
                interface_ipv6s.push(ip);
            }
        }
        response.interface_ipv6s = interface_ipv6s;
        if let Some(ip) = response.public_ipv6.map(Ipv6Addr::from)
            && peer_manager.is_easytier_managed_ipv6(&ip).await
        {
            response.public_ipv6 = None;
        }
    }
    response
}

pub(crate) struct ForeignDirectConnectorRpcRegistrar<H>
where
    H: DirectConnectorHost,
{
    host: Arc<H>,
    stun: Arc<dyn StunInfoProvider>,
}

impl<H> ForeignDirectConnectorRpcRegistrar<H>
where
    H: DirectConnectorHost,
{
    pub fn new(host: Arc<H>, stun: Arc<dyn StunInfoProvider>) -> Self {
        Self { host, stun }
    }
}

impl<H> ForeignNetworkRpcRegistrar for ForeignDirectConnectorRpcRegistrar<H>
where
    H: DirectConnectorHost,
{
    fn register_peer_rpc_services(
        &self,
        peer_rpc: &Arc<PeerRpcManager>,
        network_name: &str,
        socket_context: SocketContext,
    ) {
        peer_rpc.rpc_server().registry().register(
            GeneratedDirectConnectorRpcServer::new(
                DirectConnectorRpcHandler::new_for_foreign_network_with_stun(
                    self.host.clone(),
                    socket_context,
                    Some(self.stun.clone()),
                ),
            ),
            network_name,
        );
    }
}

#[async_trait]
impl<H> DirectConnectorRpc for DirectConnectorRpcHandler<H>
where
    H: DirectConnectorHost,
{
    type Controller = BaseController;

    async fn get_ip_list(
        &self,
        _: BaseController,
        _: GetIpListRequest,
    ) -> rpc_types::error::Result<GetIpListResponse> {
        let peer_manager = self.peer_manager.as_ref().and_then(Weak::upgrade);
        let mut response = collect_address_observations(
            self.host.as_ref(),
            peer_manager.as_deref(),
            &self.socket_context,
            self.foreign_network,
            self.stun
                .as_deref()
                .map(StunInfoProvider::get_stun_info)
                .as_ref(),
        )
        .await;
        let mapped_listeners = self.host.mapped_listeners();
        response.udp_http3_listeners = http3_listener_aliases(
            self.running_listeners.udp_http3_listener_urls(),
            &mapped_listeners,
        )
        .into_iter()
        .map(Into::into)
        .collect();
        response.listeners = mapped_listeners
            .into_iter()
            .chain(self.running_listeners.local_listener_urls())
            .map(Into::into)
            .collect();
        Ok(response)
    }

    async fn send_udp_hole_punch_packet(
        &self,
        _: BaseController,
        request: SendUdpHolePunchPacketRequest,
    ) -> rpc_types::error::Result<Void> {
        let (listener_port, connector_addrs, preferred_src_ipv6) =
            connector_addrs_from_request(request)?;
        let peer_manager = self.peer_manager.as_ref().and_then(Weak::upgrade);
        let preferred_source = match preferred_src_ipv6.map(Ipv6Addr::from) {
            Some(ip) => {
                if self.foreign_network {
                    self.host
                        .preferred_foreign_ipv6_source(ip, self.socket_context.clone())
                        .await
                } else if let Some(peer_manager) = peer_manager.as_deref()
                    && peer_manager.is_easytier_managed_ipv6(&ip).await
                {
                    None
                } else {
                    self.host
                        .preferred_ipv6_source(ip, self.socket_context.clone())
                        .await
                }
            }
            None => None,
        };

        for _ in 0..3 {
            for connector_addr in &connector_addrs {
                let result = match connector_addr {
                    SocketAddr::V4(addr) => {
                        send_v4_hole_punch_control_packet(
                            self.host.as_ref(),
                            self.socket_context.clone(),
                            listener_port,
                            *addr,
                        )
                        .await
                    }
                    SocketAddr::V6(addr) => {
                        send_v6_hole_punch_control_packet(
                            self.host.as_ref(),
                            self.socket_context.clone(),
                            listener_port,
                            *addr,
                            preferred_source,
                        )
                        .await
                    }
                };
                if let Err(error) = result {
                    tracing::debug!(?error, ?connector_addr, "send UDP punch packet failed");
                }
            }
            crate::foundation::time::sleep(Duration::from_millis(30)).await;
        }
        Ok(Void::default())
    }
}

fn connector_addrs_from_request(
    request: SendUdpHolePunchPacketRequest,
) -> rpc_types::error::Result<(u16, Vec<SocketAddr>, Option<crate::proto::common::Ipv6Addr>)> {
    let listener_port = u16::try_from(request.listener_port)
        .map_err(|_| anyhow::anyhow!("listener_port out of range: {}", request.listener_port))?;
    let mut connector_addrs = request
        .connector_addrs
        .into_iter()
        .map(SocketAddr::from)
        .collect::<Vec<_>>();
    if connector_addrs.is_empty() {
        connector_addrs.push(
            request
                .connector_addr
                .ok_or_else(|| anyhow::anyhow!("connector_addr is required"))?
                .into(),
        );
    }
    let mut deduped = Vec::with_capacity(connector_addrs.len());
    for addr in connector_addrs {
        if !deduped.contains(&addr) {
            deduped.push(addr);
        }
        if deduped.len() >= MAX_UDP_HOLE_PUNCH_CONNECTOR_ADDRS {
            break;
        }
    }
    Ok((listener_port, deduped, request.preferred_src_ipv6))
}

fn mapped_listener_port(url: &Url) -> Option<u16> {
    url.port()
        .or_else(|| crate::connectivity::protocol::protocol_default_port(url.scheme()))
}

fn http3_listener_aliases(ready_udp: Vec<Url>, mapped: &[Url]) -> Vec<Url> {
    if ready_udp.is_empty() {
        return Vec::new();
    }
    // Mapped listeners are the configured external aliases of this instance's
    // UDP sockets. Preserve their external ports, including translated ports.
    let mut aliases = Vec::new();
    for mut url in ready_udp.into_iter().chain(mapped.iter().cloned()) {
        if url.scheme() != "udp" {
            continue;
        }
        let Some(port) = mapped_listener_port(&url) else {
            continue;
        };
        if url.set_scheme("http3").is_ok()
            && url.set_port(Some(port)).is_ok()
            && !aliases.contains(&url)
        {
            aliases.push(url);
        }
    }
    aliases
}

async fn resolve_mapped_listener_addrs(
    listener: &Url,
    context: SocketContext,
    dns: &dyn DnsResolver,
) -> anyhow::Result<Vec<SocketAddr>> {
    let port = mapped_listener_port(listener)
        .ok_or_else(|| anyhow::anyhow!("listener has no default port: {listener}"))?;
    resolve_url_addrs(
        listener,
        port,
        context.with_ip_version(IpVersion::Both),
        dns,
    )
    .await
}

fn resolve_literal_url(url: &Url, ip_version: IpVersion) -> anyhow::Result<SocketAddr> {
    let port =
        mapped_listener_port(url).ok_or_else(|| anyhow::anyhow!("listener has no port: {url}"))?;
    match (url.host(), ip_version) {
        (Some(Host::Ipv4(ip)), IpVersion::V4 | IpVersion::Both) => {
            Ok(SocketAddr::new(IpAddr::V4(ip), port))
        }
        (Some(Host::Ipv6(ip)), IpVersion::V6 | IpVersion::Both) => {
            Ok(SocketAddr::new(IpAddr::V6(ip), port))
        }
        _ => anyhow::bail!("URL host does not match {ip_version:?}: {url}"),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_broadcast()
        && !ip.is_unspecified()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::P2pPolicyFlags;

    #[derive(Default)]
    struct FailingDialHost {
        attempts: Arc<std::sync::Mutex<Vec<u16>>>,
    }

    struct FailingUdpSocket(Arc<std::sync::Mutex<Vec<u16>>>);

    #[async_trait]
    impl VirtualUdpSocket for FailingUdpSocket {
        fn local_addr(&self) -> std::io::Result<SocketAddr> {
            Ok("127.0.0.1:20000".parse().unwrap())
        }

        async fn send_to(&self, _data: &[u8], addr: SocketAddr) -> std::io::Result<usize> {
            self.0.lock().unwrap().push(addr.port());
            Err(std::io::Error::other("injected UDP dial failure"))
        }

        async fn recv_from(&self, _buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
            std::future::pending().await
        }
    }

    #[async_trait]
    impl crate::socket::tcp::VirtualTcpSocketFactory for FailingDialHost {
        type Socket = crate::host::testkit::TestTcpSocket;

        async fn connect_tcp(
            &self,
            options: crate::socket::tcp::TcpConnectOptions,
        ) -> anyhow::Result<Self::Socket> {
            self.attempts
                .lock()
                .unwrap()
                .push(options.remote_addr.port());
            anyhow::bail!("injected TCP dial failure");
        }
    }

    #[async_trait]
    impl VirtualUdpSocketFactory for FailingDialHost {
        type Socket = FailingUdpSocket;

        async fn bind_udp(&self, _options: UdpBindOptions) -> anyhow::Result<Arc<Self::Socket>> {
            Ok(Arc::new(FailingUdpSocket(self.attempts.clone())))
        }
    }

    #[async_trait]
    impl ManualConnectorHost for FailingDialHost {
        async fn local_addr_for_remote(
            &self,
            _remote_addr: SocketAddr,
            _context: SocketContext,
        ) -> anyhow::Result<SocketAddr> {
            Ok("127.0.0.1:0".parse().unwrap())
        }

        async fn interface_addrs(
            &self,
        ) -> anyhow::Result<super::super::manual::ManualInterfaceAddrs> {
            Ok(super::super::manual::ManualInterfaceAddrs {
                interface_ipv4s: Vec::new(),
                interface_ipv6s: Vec::new(),
                public_ipv6: None,
            })
        }
    }

    #[async_trait]
    impl DirectConnectorHost for FailingDialHost {
        async fn collect_ip_addrs(&self, _context: &SocketContext) -> GetIpListResponse {
            GetIpListResponse::default()
        }

        fn mapped_listeners(&self) -> Vec<Url> {
            Vec::new()
        }

        fn is_local_ip(&self, _ip: &IpAddr) -> bool {
            false
        }

        async fn preferred_ipv6_source(
            &self,
            _ip: Ipv6Addr,
            _context: SocketContext,
        ) -> Option<PreferredIpv6Source> {
            None
        }
    }

    #[async_trait]
    impl StunInfoProvider for FailingDialHost {
        fn get_stun_info(&self) -> crate::proto::common::StunInfo {
            Default::default()
        }

        async fn get_udp_port_mapping(&self, _port: u16) -> anyhow::Result<SocketAddr> {
            panic!("private direct targets must not request STUN mappings")
        }

        async fn get_tcp_port_mapping(&self, _port: u16) -> anyhow::Result<SocketAddr> {
            panic!("private direct targets must not request STUN mappings")
        }

        fn update_stun_info(&self) {}
    }

    #[async_trait]
    impl StunSocketMapper<FailingUdpSocket> for FailingDialHost {
        async fn get_udp_port_mapping_with_socket(
            &self,
            _socket: Arc<FailingUdpSocket>,
        ) -> anyhow::Result<SocketAddr> {
            panic!("private direct targets must not request STUN mappings")
        }
    }

    #[async_trait]
    impl ClientProtocolUpgrader<crate::host::testkit::TestTcpSocket> for FailingDialHost {
        fn supports_scheme(&self, scheme: &str) -> bool {
            matches!(scheme, "tcp" | "udp" | "wss" | "http3")
        }

        async fn upgrade_client(
            &self,
            _connected: ConnectedTransport<crate::host::testkit::TestTcpSocket>,
            requested_url: Url,
        ) -> anyhow::Result<Box<dyn Tunnel>> {
            // HTTP3 opens a classified UDP session without a mux SYN, so its
            // actual protocol dial begins here instead of at socket.send_to.
            assert_eq!(requested_url.scheme(), "http3");
            self.attempts
                .lock()
                .unwrap()
                .push(requested_url.port().unwrap());
            anyhow::bail!("injected HTTP3 dial failure");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn direct_connect_loop_dials_preferred_transport_before_failed_fallback() {
        use crate::{
            config::peers::PeerRuntimeSnapshot,
            host::{packet::host_packet_channel, testkit::TestDns},
            peers::peer_manager::PortablePeerManagerConfig,
        };

        for (default_protocol, strict_disguise, expected_ports) in [
            (Some("tcp"), false, [11010, 11011]),
            (Some("udp"), false, [11011, 11010]),
            (Some("tcp"), true, [11012, 11014]),
            (Some("udp"), true, [11014, 11012]),
            (None, false, [11011, 11010]),
            (None, true, [11014, 11012]),
            (Some(""), false, [11011, 11010]),
            (Some("  "), true, [11014, 11012]),
        ] {
            let mut config = PortablePeerManagerConfig::new(PeerRuntimeSnapshot::default().runtime);
            let mut options = DirectConnectorOptions::default();
            if let Some(default_protocol) = default_protocol {
                config.snapshot.flags.default_protocol = default_protocol.to_owned();
                options.default_protocol = default_protocol.to_owned();
            }
            config.snapshot.flags.only_use_wss_http3_for_hole_punching = strict_disguise;
            let (packet_tx, _packet_rx) = host_packet_channel();
            let peers =
                Arc::new(PeerManagerCore::new_portable_for_test(config, packet_tx).unwrap());
            let host = Arc::new(FailingDialHost::default());
            let manager = DirectConnectorManager::new_with_running_listeners(
                peers.clone(),
                host.clone(),
                Arc::new(ProtectedTcpPortRegistry::default()),
                host.clone(),
                Arc::new(NoLocalListeners),
                Arc::new(TestDns),
                host.clone(),
                options,
            );
            let listeners = [
                "udp://192.168.1.2:11011",
                "wss://192.168.1.2:11012",
                "tcp://192.168.1.2:11010",
                "tcp://192.168.1.2:11010",
            ]
            .into_iter()
            .map(|url| url.parse().unwrap())
            .collect();
            manager
                .data
                .try_direct_connect_with_ip_list(
                    2,
                    GetIpListResponse {
                        listeners,
                        udp_http3_listeners: vec!["http3://192.168.1.2:11014".parse().unwrap()],
                        ..Default::default()
                    },
                )
                .await
                .unwrap();

            // Record real transport attempts, including all four failed
            // retries, rather than reconstructing the sorted listener list.
            let expected = expected_ports
                .into_iter()
                .flat_map(|port| std::iter::repeat_n(port, 4))
                .collect::<Vec<_>>();
            assert_eq!(
                *host.attempts.lock().unwrap(),
                expected,
                "{default_protocol:?}, strict_disguise={strict_disguise}"
            );
            peers.clear_resources().await;
        }
    }

    #[test]
    fn http3_aliases_preserve_mapped_ports_and_disappear_without_ready_udp() {
        let mapped = vec![
            "udp://203.0.113.1:23456".parse().unwrap(),
            "udp://[2001:db8::1]:34567".parse().unwrap(),
            "tcp://203.0.113.1:23456".parse().unwrap(),
        ];
        assert!(http3_listener_aliases(Vec::new(), &mapped).is_empty());
        let aliases = http3_listener_aliases(
            vec![
                "udp://0.0.0.0:11010".parse().unwrap(),
                "udp://[::]:11010".parse().unwrap(),
            ],
            &mapped,
        );
        assert_eq!(
            aliases,
            vec![
                "http3://0.0.0.0:11010".parse::<Url>().unwrap(),
                "http3://[::]:11010".parse().unwrap(),
                "http3://203.0.113.1:23456".parse().unwrap(),
                "http3://[2001:db8::1]:34567".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn http3_capability_field_keeps_official_listener_wire_compatible() {
        use prost::Message;

        #[derive(Clone, PartialEq, Message)]
        struct OfficialResponse {
            #[prost(message, repeated, tag = "5")]
            listeners: Vec<crate::proto::common::Url>,
        }
        let raw: crate::proto::common::Url = "udp://127.0.0.1:11010".parse().unwrap();
        let response = GetIpListResponse {
            listeners: vec![raw.clone()],
            udp_http3_listeners: vec!["http3://127.0.0.1:11010".parse().unwrap()],
            ..Default::default()
        };
        let official = OfficialResponse::decode(response.encode_to_vec().as_slice()).unwrap();
        assert_eq!(official.listeners, vec![raw]);
        let decoded = GetIpListResponse::decode(official.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.listeners, official.listeners);
        assert!(decoded.udp_http3_listeners.is_empty());
    }

    impl<H> DirectConnectorRpcHandler<H>
    where
        H: DirectConnectorHost,
    {
        pub(crate) fn new_with_stun(
            host: Arc<H>,
            socket_context: SocketContext,
            stun: Option<Arc<dyn StunInfoProvider>>,
        ) -> Self {
            let running_listeners = Arc::new(NoLocalListeners);
            Self {
                host,
                peer_manager: None,
                running_listeners,
                socket_context,
                foreign_network: false,
                stun,
            }
        }
    }

    #[test]
    fn direct_listener_consumption_follows_protocol_preference() {
        for (default_protocol, use_disguise, expected) in [
            ("udp", true, vec!["http3", "wss", "udp", "udp", "tcp"]),
            ("tcp", true, vec!["wss", "http3", "tcp", "udp", "udp"]),
            ("udp", false, vec!["udp", "udp", "tcp"]),
            ("tcp", false, vec!["tcp", "udp", "udp"]),
        ] {
            let listeners = [
                "tcp://192.0.2.1:11010",
                "http3://192.0.2.1:11014",
                "udp://192.0.2.1:11010",
                "wss://192.0.2.1:11012",
                "udp://192.0.2.1:11011",
                "udp://192.0.2.1:11010",
            ]
            .into_iter()
            .map(|url| url.parse::<Url>().unwrap())
            .filter(|url| use_disguise || matches!(url.scheme(), "tcp" | "udp"))
            .collect();
            let consumed = ordered_direct_listeners(listeners, default_protocol, use_disguise)
                .map(|listener| listener.scheme().to_owned())
                .collect::<Vec<_>>();

            assert_eq!(
                consumed, expected,
                "{default_protocol}, disguise={use_disguise}"
            );
        }
    }

    #[test]
    fn faketcp_uses_specialized_tcp_socket_without_interface_binding() {
        let transport =
            direct_transport_from_url(&"faketcp://127.0.0.1:11013".parse().unwrap()).unwrap();

        assert_eq!(transport, IpTransport::Tcp(TcpSocketPurpose::FakeTcp));
        assert!(!transport.supports_interface_bind());
        assert!(!transport.is_udp());
        // Byte-stream endpoints are not direct-dialable and must be rejected.
        assert!(direct_transport_from_url(&"ring://local".parse().unwrap()).is_err());
    }

    #[test]
    fn connector_address_request_deduplicates_and_caps() {
        let mut request = SendUdpHolePunchPacketRequest {
            listener_port: 11010,
            ..Default::default()
        };
        for port in 1..=20 {
            request
                .connector_addrs
                .push(SocketAddr::from(([127, 0, 0, 1], port)).into());
        }
        request
            .connector_addrs
            .push(SocketAddr::from(([127, 0, 0, 1], 1)).into());

        let (_, addrs, _) = connector_addrs_from_request(request).unwrap();
        assert_eq!(addrs.len(), MAX_UDP_HOLE_PUNCH_CONNECTOR_ADDRS);
        assert_eq!(addrs[0], SocketAddr::from(([127, 0, 0, 1], 1)));
    }

    #[test]
    fn expiring_set_removes_expired_entries() {
        let set = ExpiringSet::default();
        set.insert(7u32, Duration::ZERO);
        assert!(!set.contains(&7));
    }

    #[test]
    fn disguise_preference_retries_with_an_existing_raw_direct_connection() {
        // lazy_p2p keeps the background (static) allowance off, so only the
        // on-demand branch of the shared engine gate can admit the peer.
        let policy = P2pPolicyFlags {
            lazy_p2p: true,
            ..Default::default()
        };
        let allowed = |has_recent_traffic: bool, has_direct_connection: bool, satisfied: bool| {
            should_collect_direct_peer(
                false,
                false,
                satisfied,
                p2p_engine_gate(
                    None,
                    false,
                    &policy,
                    has_recent_traffic,
                    has_direct_connection,
                    satisfied,
                ),
            )
        };
        assert!(allowed(false, true, false));
        assert!(!allowed(false, true, true));
        assert!(!allowed(false, false, false));
        assert!(allowed(true, false, false));
        assert!(!should_collect_direct_peer(true, false, false, true));
        assert!(!should_collect_direct_peer(false, true, false, true));
    }
}
