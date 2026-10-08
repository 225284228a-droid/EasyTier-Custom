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

// ---- Mock hole-punch host with an ordered event log ----

/// Socket type for the mock host: satisfies the stream contract without
/// carrying data; reads hit EOF immediately.
struct MockPunchSocket(SocketAddr, SocketAddr, Option<Arc<AtomicUsize>>);

impl Drop for MockPunchSocket {
    fn drop(&mut self) {
        if let Some(active_sockets) = &self.2 {
            active_sockets.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

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
    active_sockets: Arc<AtomicUsize>,
}

impl MockHolePunchHost {
    fn new(connect_mode: MockConnectMode) -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            connect_mode,
            accept_queue: Arc::new(Mutex::new(VecDeque::new())),
            active_sockets: Arc::new(AtomicUsize::new(0)),
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
            MockConnectMode::Succeed => {
                self.active_sockets.fetch_add(1, Ordering::SeqCst);
                Ok(MockPunchSocket(
                    options.bind.local_addr.unwrap_or(options.remote_addr),
                    options.remote_addr,
                    Some(self.active_sockets.clone()),
                ))
            }
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
    execution_errors: Arc<Mutex<VecDeque<&'static str>>>,
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
        if let Some(message) = self.execution_errors.lock().unwrap().pop_front() {
            return Err(anyhow::anyhow!(message).into());
        }
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
            tcp_satisfied: false,
            wss_satisfied: false,
            has_recent_traffic: true,
            peer_disguise_flags: PeerDisguiseP2pFlags {
                prefer_wss_http3_for_p2p: self.policy.prefer_wss_http3_for_p2p,
                ..Default::default()
            },
        }]
    }

    fn rpc_stub(
        &self,
        _dst_peer_id: PeerId,
    ) -> Box<dyn TcpHolePunchRpc<Controller = BaseController> + Send + Sync + 'static> {
        Box::new(MockTcpHolePunchRpc {
            response: self.rpc.response.clone(),
            requests: self.rpc.requests.clone(),
            execution_errors: self.rpc.execution_errors.clone(),
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
        execution_errors: Arc::new(Mutex::new(VecDeque::new())),
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

struct PendingTransportSink;

#[async_trait]
impl TcpHolePunchTransportSink for PendingTransportSink {
    type ConnectedSocket = MockPunchSocket;
    type AcceptedSocket = MockPunchSocket;

    fn supports_wss_hole_punching(&self) -> bool {
        true
    }

    async fn add_connected_transport(
        &self,
        socket: MockPunchSocket,
        _requested_url: url::Url,
        _admission: TcpHolePunchAdmission,
    ) -> Result<(), TcpHolePunchTransportError> {
        std::future::pending::<()>().await;
        drop(socket);
        Ok(())
    }

    async fn add_accepted_transport(
        &self,
        _socket: MockPunchSocket,
        _local_url: url::Url,
    ) -> Result<(), TcpHolePunchTransportError> {
        unreachable!("deadline tests only dial")
    }
}

async fn assert_active_sockets(host: &MockHolePunchHost, expected: usize) {
    for _ in 0..20 {
        if host.active_sockets.load(Ordering::SeqCst) == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(host.active_sockets.load(Ordering::SeqCst), expected);
}

#[tokio::test(start_paused = true)]
async fn single_dial_deadline_cancels_pending_upgrade_and_releases_socket() {
    let host = MockHolePunchHost::new(MockConnectMode::Succeed);
    let task = tokio::spawn(try_connect_to_remote(
        host.clone(),
        Arc::new(PendingTransportSink),
        TcpHolePunchDialOptions {
            remote_mapped_addr: "198.51.100.1:443".parse().unwrap(),
            local_port: MOCK_LOCAL_PORT,
            context: SocketContext::default(),
            admission: TcpHolePunchAdmission::Client,
            max_attempts: 1,
            scheme: "wss",
        },
    ));
    assert_active_sockets(&host, 1).await;

    tokio::time::advance(Duration::from_secs(10)).await;
    let error = task.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("window elapsed"));
    assert_eq!(host.active_sockets.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn responder_deadline_cancels_all_pending_upgrades_and_releases_sockets() {
    let host = MockHolePunchHost::new(MockConnectMode::Succeed);
    let task = tokio::spawn(connect_as_symmetric_responder(
        host.clone(),
        Arc::new(PendingTransportSink),
        "198.51.100.1:443".parse().unwrap(),
        MOCK_LOCAL_PORT,
        SocketContext::default(),
        TcpHolePunchAdmission::Client,
        "wss",
    ));
    assert_active_sockets(&host, SYMMETRIC_RESPONDER_SOCKET_COUNT).await;

    tokio::time::advance(SYMMETRIC_RESPONDER_HOLD).await;
    assert!(task.await.unwrap().is_err());
    assert_eq!(host.active_sockets.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn spray_deadline_cancels_all_pending_upgrades_and_releases_sockets() {
    let host = MockHolePunchHost::new(MockConnectMode::Succeed);
    let task = tokio::spawn(spray_connect_to_predicted_ports(
        host.clone(),
        Arc::new(PendingTransportSink),
        vec![
            "198.51.100.1:443".parse().unwrap(),
            "198.51.100.1:444".parse().unwrap(),
            "198.51.100.1:445".parse().unwrap(),
        ],
        MOCK_LOCAL_PORT,
        SocketContext::default(),
        TcpHolePunchAdmission::Server,
        "wss",
    ));
    assert_active_sockets(&host, 3).await;

    tokio::time::advance(SPRAY_WINDOW).await;
    assert!(task.await.unwrap().is_err());
    assert_eq!(host.active_sockets.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn strict_wss_retries_transient_execution_error_without_blacklisting() {
    let host = MockHolePunchHost::new(MockConnectMode::Succeed);
    let (mut data, rpc, tunnel_sink) = mock_connector_data(
        host,
        NatType::FullCone,
        P2pPolicyFlags {
            only_use_wss_http3_for_hole_punching: true,
            ..Default::default()
        },
        TcpHolePunchResponse {
            scheme: "wss".into(),
            ..legacy_response()
        },
    );
    Arc::get_mut(&mut data).unwrap().supports_wss_hole_punching = true;
    rpc.execution_errors
        .lock()
        .unwrap()
        .push_back("failed to get tcp port mapping: transient STUN error");

    let error = data.do_punch_as_initiator(2).await.unwrap_err();
    assert!(error.to_string().contains("transient STUN error"));
    assert!(!data.blacklist.contains(2));
    data.do_punch_as_initiator(2).await.unwrap();

    let requests = rpc.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.scheme == "wss"));
    assert_eq!(tunnel_sink.servers.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn strict_wss_blacklists_after_repeated_execution_errors() {
    let host = MockHolePunchHost::new(MockConnectMode::Succeed);
    let (mut data, rpc, _tunnel_sink) = mock_connector_data(
        host,
        NatType::FullCone,
        P2pPolicyFlags {
            only_use_wss_http3_for_hole_punching: true,
            ..Default::default()
        },
        TcpHolePunchResponse {
            scheme: "wss".into(),
            ..legacy_response()
        },
    );
    Arc::get_mut(&mut data).unwrap().supports_wss_hole_punching = true;
    for _ in 0..WSS_EXCHANGE_FAILURE_BLACKLIST_THRESHOLD {
        rpc.execution_errors
            .lock()
            .unwrap()
            .push_back("this node does not support WSS hole punching");
    }

    for _ in 0..WSS_EXCHANGE_FAILURE_BLACKLIST_THRESHOLD - 1 {
        assert!(data.do_punch_as_initiator(2).await.is_err());
        assert!(!data.blacklist.contains(2));
    }
    assert!(data.do_punch_as_initiator(2).await.is_err());
    assert!(data.blacklist.contains(2));
}

#[tokio::test]
async fn preferred_wss_execution_error_preserves_raw_tcp_fallback() {
    let host = MockHolePunchHost::new(MockConnectMode::Succeed);
    let (mut data, rpc, _tunnel_sink) = mock_connector_data(
        host,
        NatType::FullCone,
        P2pPolicyFlags {
            prefer_wss_http3_for_p2p: true,
            ..Default::default()
        },
        legacy_response(),
    );
    Arc::get_mut(&mut data).unwrap().supports_wss_hole_punching = true;
    rpc.execution_errors
        .lock()
        .unwrap()
        .push_back("this node does not support WSS hole punching");

    data.do_punch_as_initiator(2).await.unwrap();

    let requests = rpc.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].scheme, "wss");
    assert!(requests[1].scheme.is_empty());
    assert!(!data.blacklist.contains(2));
}

#[tokio::test]
async fn strict_wss_preserves_blacklist_for_explicit_scheme_mismatch() {
    let host = MockHolePunchHost::new(MockConnectMode::Succeed);
    let (mut data, _rpc, _tunnel_sink) = mock_connector_data(
        host,
        NatType::FullCone,
        P2pPolicyFlags {
            only_use_wss_http3_for_hole_punching: true,
            ..Default::default()
        },
        legacy_response(),
    );
    Arc::get_mut(&mut data).unwrap().supports_wss_hole_punching = true;

    assert!(data.do_punch_as_initiator(2).await.is_err());
    assert!(data.blacklist.contains(2));
}

#[test]
fn invalid_rpc_service_preserves_blacklisting() {
    let blacklist = TcpHolePunchBlacklist::new();
    let result = handle_rpc_result::<()>(
        Err(rpc_types::error::Error::InvalidServiceKey(
            "TcpHolePunchRpc".into(),
            "TcpHolePunchRpc".into(),
        )),
        2,
        &blacklist,
    );
    assert!(result.is_err());
    assert!(blacklist.contains(2));
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
        None,
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
        None,
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
        None,
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
        None,
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
        execution_errors: Arc::new(Mutex::new(VecDeque::new())),
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
        TcpHolePunchRpc::exchange_mapped_addr(&*server, BaseController::default(), request).await;
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
            execution_errors: Arc::new(Mutex::new(VecDeque::new())),
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
