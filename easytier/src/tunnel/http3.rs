//! HTTP/3 duplex streaming POST tunnels; control streams and QPACK use h3.
pub(crate) use super::quic::QuicUdpSessionSocket as Http3UdpSessionSocket;
use super::{
    insecure_tls::{SkipServerVerification, get_insecure_tls_cert, sni_from_url},
    quic::{session_common, window_source},
    traffic_disguise::{PacedSink, TrafficStyle},
};
use crate::proto::common::TunnelInfo;
use anyhow::Context;
use bytes::{Buf, Bytes};
use easytier_core::{
    connectivity::{
        protocol::{ServerProtocolAdmission, ServerTunnelAcceptor},
        transport::ConnectedUdpSession,
    },
    packet::{PEER_MANAGER_HEADER_SIZE, PacketType, ZCPacket},
    socket::udp::UdpSession,
    tunnel::{
        Tunnel, TunnelError,
        framed::{FramedReader, FramedWriter, TcpZCPacketToBytes, ZCPacketToBytes},
        wrapper::TunnelWrapper,
    },
};
use futures::StreamExt as _;
use quinn::{
    AsyncUdpSocket, ClientConfig, EndpointConfig, ServerConfig, TransportConfig,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinSet,
};
use tokio_util::task::AbortOnDropHandle;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_BUFFER_SIZE: usize = 64 * 1024;
const PARALLEL_FRAME_OVERHEAD: usize = 40;
const MAX_HTTP3_FRAME_SIZE: usize = session_common::MAX_TUNNEL_BODY_SIZE + PARALLEL_FRAME_OVERHEAD;
type H3ServerConnection = h3::server::Connection<h3_quinn::Connection, Bytes>;
type H3ServerStream = h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

pub fn transport_config() -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    config
        .max_concurrent_uni_streams(16u32.into())
        .max_concurrent_bidi_streams(100u32.into())
        .stream_receive_window((4u32 * 1024 * 1024).into())
        .receive_window((8u32 * 1024 * 1024).into())
        .send_window(8 * 1024 * 1024)
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .initial_mtu(1200)
        .min_mtu(1200)
        .enable_segmentation_offload(true)
        .congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    Arc::new(config)
}
fn build_rustls_client_config() -> anyhow::Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(SkipServerVerification::new(provider))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h3".to_vec()];
    config.enable_early_data = false;
    Ok(config)
}
fn build_rustls_server_config() -> anyhow::Result<rustls::ServerConfig> {
    let (certificates, private_key) = get_insecure_tls_cert();
    let mut config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(certificates, private_key)?;
    config.alpn_protocols = vec![b"h3".to_vec()];
    config.max_early_data_size = 0;
    Ok(config)
}
pub fn server_config() -> anyhow::Result<ServerConfig> {
    let mut config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(
        build_rustls_server_config()?,
    )?));
    config.transport_config(transport_config());
    Ok(config)
}
pub fn client_config() -> anyhow::Result<ClientConfig> {
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(
        build_rustls_client_config()?,
    )?));
    config.transport_config(transport_config());
    Ok(config)
}
pub fn endpoint_config() -> EndpointConfig {
    let mut config = EndpointConfig::default();
    config.max_udp_payload_size(1200).unwrap();
    config
}
fn resolve_sni(url: &url::Url) -> String {
    let param = url
        .query_pairs()
        .find(|(key, _)| key == "sni")
        .map(|(_, value)| value.into_owned());
    sni_from_url(url, param.as_deref())
}
fn request_uri(url: &url::Url, style: TrafficStyle) -> anyhow::Result<http::Uri> {
    // `url` forbids changing a non-special scheme into the special HTTPS
    // scheme in place. Reparse the authority/path with HTTPS instead.
    let mut request = url::Url::parse(&format!("https{}", &url[url::Position::AfterScheme..]))?;
    let pairs: Vec<_> = request
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if let Some((_, path)) = pairs.iter().find(|(key, _)| key == "path") {
        request.set_path(path);
    } else if request.path() == "/" {
        request.set_path(match style {
            TrafficStyle::Speedtest => "/api/upload",
            TrafficStyle::Video => "/media/upload",
        });
    }
    let authority = if let Some((_, host)) = pairs
        .iter()
        .find(|(key, value)| key == "host" && !value.is_empty())
    {
        anyhow::ensure!(
            !host
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '@' | '/' | '?' | '#')),
            "invalid HTTP3 Host"
        );
        let authority: http::uri::Authority = host.parse()?;
        authority.to_string()
    } else {
        request[url::Position::BeforeHost..url::Position::AfterPort].to_owned()
    };
    request.set_query(None);
    let retained: Vec<_> = pairs
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "sni" | "host" | "path" | "ua" | "accept_language" | "padding" | "traffic_disguise"
            )
        })
        .collect();
    if !retained.is_empty() {
        request
            .query_pairs_mut()
            .extend_pairs(retained.iter().map(|pair| (&pair.0, &pair.1)));
    }
    Ok(http::Uri::builder()
        .scheme("https")
        .authority(authority.as_str())
        .path_and_query(&request[url::Position::BeforePath..])
        .build()?)
}
/// Last tunnel half owns cancellation, endpoint lifetime and connection close.
struct H3Lifetime {
    connection: session_common::ConnWrapper,
    tasks: Mutex<Vec<AbortOnDropHandle<()>>>,
}
impl H3Lifetime {
    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks
            .lock()
            .unwrap()
            .push(AbortOnDropHandle::new(tokio::spawn(future)));
    }
}
impl Drop for H3Lifetime {
    fn drop(&mut self) {
        self.connection.conn.close(0x100u32.into(), b"");
        self.tasks.get_mut().unwrap().clear();
    }
}
#[async_trait::async_trait]
trait BodySend: Send + 'static {
    async fn data(&mut self, data: Bytes) -> anyhow::Result<()>;
    async fn finish(&mut self) -> anyhow::Result<()>;
}
#[async_trait::async_trait]
trait BodyRecv: Send + 'static {
    async fn data(&mut self) -> anyhow::Result<Option<Bytes>>;
}
macro_rules! impl_http_body {
    ($kind:ident) => {
        #[async_trait::async_trait]
        impl BodySend for h3::$kind::RequestStream<h3_quinn::SendStream<Bytes>, Bytes> {
            async fn data(&mut self, data: Bytes) -> anyhow::Result<()> {
                Ok(self.send_data(data).await?)
            }
            async fn finish(&mut self) -> anyhow::Result<()> {
                Ok(h3::$kind::RequestStream::finish(self).await?)
            }
        }
        #[async_trait::async_trait]
        impl BodyRecv for h3::$kind::RequestStream<h3_quinn::RecvStream, Bytes> {
            async fn data(&mut self) -> anyhow::Result<Option<Bytes>> {
                Ok(self.recv_data().await?.map(|mut data| {
                    let len = data.remaining();
                    data.copy_to_bytes(len)
                }))
            }
        }
    };
}
impl_http_body!(client);
impl_http_body!(server);
struct CheckedFrameConverter;

fn validate_frame(packet: &ZCPacket) -> Result<(), TunnelError> {
    let body_size = PEER_MANAGER_HEADER_SIZE + packet.payload_len();
    let parallel = packet.peer_manager_header().is_some_and(|header| {
        header.packet_type == PacketType::ParallelData as u8
            || header.packet_type == PacketType::ParallelLossyData as u8
    });
    let limit =
        session_common::MAX_TUNNEL_BODY_SIZE + if parallel { PARALLEL_FRAME_OVERHEAD } else { 0 };
    if body_size > limit
        || (parallel && body_size < PARALLEL_FRAME_OVERHEAD + PEER_MANAGER_HEADER_SIZE)
    {
        return Err(TunnelError::InvalidPacket(
            "HTTP3 tunnel frame exceeds maximum size or is truncated".to_owned(),
        ));
    }
    Ok(())
}

impl ZCPacketToBytes for CheckedFrameConverter {
    fn zcpacket_into_bytes(&self, packet: ZCPacket) -> Result<Bytes, TunnelError> {
        validate_frame(&packet)?;
        TcpZCPacketToBytes.zcpacket_into_bytes(packet)
    }
}
fn body_tunnel(
    mut send: impl BodySend,
    mut recv: impl BodyRecv,
    lifetime: Arc<H3Lifetime>,
    style: TrafficStyle,
    info: TunnelInfo,
) -> Box<dyn Tunnel> {
    let (read, mut receive_writer) = tokio::io::duplex(BODY_BUFFER_SIZE);
    let (mut send_reader, write) = tokio::io::duplex(BODY_BUFFER_SIZE);
    let connection = lifetime.connection.conn.clone();
    lifetime.spawn(async move {
        let result: anyhow::Result<()> = async {
            let mut buffer = vec![0; 16 * 1024];
            loop {
                let count = send_reader.read(&mut buffer).await?;
                if count == 0 {
                    return send.finish().await;
                }
                send.data(Bytes::copy_from_slice(&buffer[..count])).await?;
            }
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(?error, "HTTP3 body send ended");
            connection.close(0x102u32.into(), b"");
        }
    });
    let connection = lifetime.connection.conn.clone();
    lifetime.spawn(async move {
        let result: anyhow::Result<()> = async {
            while let Some(data) = recv.data().await? {
                receive_writer.write_all(&data).await?;
            }
            receive_writer.shutdown().await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(?error, "HTTP3 body receive ended");
            connection.close(0x102u32.into(), b"");
        }
    });
    let bandwidth_source = window_source(&lifetime.connection.conn);
    Box::new(
        TunnelWrapper::new(
            FramedReader::new_with_associate_data(
                read,
                MAX_HTTP3_FRAME_SIZE,
                Some(Box::new(lifetime.clone())),
            )
            .map(|result| {
                result.and_then(|packet| {
                    validate_frame(&packet)?;
                    Ok(packet)
                })
            }),
            PacedSink::new(
                FramedWriter::new_with_converter_and_associate_data(
                    write,
                    CheckedFrameConverter,
                    Some(Box::new(lifetime)),
                ),
                style,
            ),
            Some(info),
        )
        .with_bandwidth_source(Some(bandwidth_source)),
    )
}
pub(crate) async fn upgrade_connected(
    connected: ConnectedUdpSession,
    remote_url: url::Url,
) -> Result<Box<dyn Tunnel>, TunnelError> {
    let style = TrafficStyle::from_url(&remote_url);
    let socket = Arc::new(Http3UdpSessionSocket::new(connected)?);
    let local_addr = socket.local_addr()?;
    let remote_addr = socket.peer_addr();
    let mut endpoint = session_common::build_session_endpoint(socket, None, endpoint_config())?;
    endpoint.set_default_client_config(client_config()?);
    let connecting = endpoint
        .connect(remote_addr, &resolve_sni(&remote_url))
        .map_err(anyhow::Error::new)?;
    let connection = tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
        .await
        .map_err(TunnelError::Timeout)?
        .context("HTTP3 TLS connection failed")?;
    let resolved_remote_addr = connection.remote_address();
    let lifetime = Arc::new(H3Lifetime {
        connection: session_common::ConnWrapper::new(connection.clone(), endpoint),
        tasks: Mutex::new(Vec::new()),
    });
    let (mut driver, mut requests) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        h3::client::builder()
            .max_field_section_size(16 * 1024)
            .build(h3_quinn::Connection::new(connection)),
    )
    .await
    .map_err(TunnelError::Timeout)?
    .context("HTTP3 control stream setup failed")?;
    let requests_keepalive = requests.clone();
    lifetime.spawn(async move {
        // h3 closes the entire connection when its last SendRequest drops,
        // even while a duplex request stream still exists.
        let _requests_keepalive = requests_keepalive;
        let _ = driver.wait_idle().await;
    });
    let mut request = http::Request::builder()
        .method(http::Method::POST)
        .uri(request_uri(&remote_url, style)?)
        .header(http::header::CONTENT_TYPE, "application/octet-stream")
        .header(http::header::CACHE_CONTROL, "no-store")
        .header(http::header::ACCEPT, "*/*");
    for (key, value) in remote_url.query_pairs() {
        if key == "ua" {
            request = request.header(http::header::USER_AGENT, value.as_ref());
        }
        if key == "accept_language" {
            request = request.header(http::header::ACCEPT_LANGUAGE, value.as_ref());
        }
    }
    let request = request.body(()).map_err(anyhow::Error::new)?;
    let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, requests.send_request(request))
        .await
        .map_err(TunnelError::Timeout)?
        .context("HTTP3 POST failed")?;
    let response = tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.recv_response())
        .await
        .map_err(TunnelError::Timeout)?
        .context("HTTP3 response failed")?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!("HTTP3 POST rejected: {}", response.status()).into());
    }
    let (send, recv) = stream.split();
    let info = TunnelInfo {
        tunnel_type: "http3".to_owned(),
        local_addr: Some(
            super::build_url_from_socket_addr(&local_addr.to_string(), "http3").into(),
        ),
        remote_addr: Some(remote_url.into()),
        resolved_remote_addr: Some(
            super::build_url_from_socket_addr(&resolved_remote_addr.to_string(), "http3").into(),
        ),
    };
    Ok(body_tunnel(send, recv, lifetime, style, info))
}
async fn ordinary_response(
    request: &http::Request<()>,
    stream: &mut H3ServerStream,
) -> anyhow::Result<()> {
    let status = if request.method() == http::Method::GET || request.method() == http::Method::HEAD
    {
        http::StatusCode::OK
    } else {
        http::StatusCode::METHOD_NOT_ALLOWED
    };
    stream
        .send_response(
            http::Response::builder()
                .status(status)
                .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                .header(http::header::CACHE_CONTROL, "no-store")
                .body(())?,
        )
        .await?;
    if request.method() != http::Method::HEAD {
        stream.send_data(Bytes::from_static(b"OK\n")).await?;
    }
    stream.finish().await?;
    Ok(())
}
async fn serve_remaining_requests(mut driver: H3ServerConnection) {
    while let Ok(Some(resolver)) = driver.accept().await {
        let Ok((request, mut stream)) = resolver.resolve_request().await else {
            continue;
        };
        if ordinary_response(&request, &mut stream).await.is_err() {
            break;
        }
    }
}
async fn finish_http3_tunnel(
    connecting: quinn::Connecting,
    endpoint: quinn::Endpoint,
    local_url: url::Url,
    _permit: OwnedSemaphorePermit,
) -> Result<Box<dyn Tunnel>, TunnelError> {
    let connection = tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
        .await
        .map_err(TunnelError::Timeout)?
        .context("HTTP3 incoming TLS connection failed")?;
    let remote_addr = connection.remote_address();
    let lifetime = Arc::new(H3Lifetime {
        connection: session_common::ConnWrapper::new(connection.clone(), endpoint),
        tasks: Mutex::new(Vec::new()),
    });
    let mut driver: H3ServerConnection = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        h3::server::builder()
            .max_field_section_size(16 * 1024)
            .build(h3_quinn::Connection::new(connection)),
    )
    .await
    .map_err(TunnelError::Timeout)?
    .context("HTTP3 server control stream setup failed")?;
    let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        loop {
            let resolver = driver
                .accept()
                .await?
                .ok_or_else(|| anyhow::anyhow!("HTTP3 connection closed before POST"))?;
            let (request, mut stream) = resolver.resolve_request().await?;
            if request.method() == http::Method::POST {
                break Ok::<_, anyhow::Error>(stream);
            }
            ordinary_response(&request, &mut stream).await?;
        }
    })
    .await
    .map_err(TunnelError::Timeout)??;
    stream
        .send_response(
            http::Response::builder()
                .status(200)
                .header(http::header::CONTENT_TYPE, "application/octet-stream")
                .header(http::header::CACHE_CONTROL, "no-store")
                .body(())
                .map_err(anyhow::Error::new)?,
        )
        .await
        .context("HTTP3 POST response failed")?;
    let (send, recv) = stream.split();
    lifetime.spawn(serve_remaining_requests(driver));
    let style = TrafficStyle::from_url(&local_url);
    let remote_url = super::build_url_from_socket_addr(&remote_addr.to_string(), "http3");
    let info = TunnelInfo {
        tunnel_type: "http3".to_owned(),
        local_addr: Some(local_url.into()),
        remote_addr: Some(remote_url.clone().into()),
        resolved_remote_addr: Some(remote_url.into()),
    };
    Ok(body_tunnel(send, recv, lifetime, style, info))
}
pub(crate) struct Http3AcceptedSession {
    completed: mpsc::Receiver<Result<Box<dyn Tunnel>, TunnelError>>,
    _task: AbortOnDropHandle<()>,
}
impl Http3AcceptedSession {
    pub(crate) fn new(
        session: UdpSession,
        local_url: url::Url,
        admission: ServerProtocolAdmission,
    ) -> Result<Self, TunnelError> {
        let (active_session, handshakes) = admission.into_parts();
        Self::new_with_admission_parts(session, local_url, active_session, handshakes)
    }
    fn new_with_admission_parts(
        session: UdpSession,
        local_url: url::Url,
        active_session: OwnedSemaphorePermit,
        handshakes: Arc<Semaphore>,
    ) -> Result<Self, TunnelError> {
        let socket = Arc::new(Http3UdpSessionSocket::from_accepted(
            session,
            active_session,
        )?);
        let endpoint = session_common::build_session_endpoint(
            socket,
            Some(server_config()?),
            endpoint_config(),
        )?;
        let (completed_tx, completed) = mpsc::channel(16);
        let task = tokio::spawn(async move {
            let mut workers = JoinSet::new();
            let mut incoming: Option<quinn::Incoming> = None;
            loop {
                tokio::select! {
                    Some(result) = workers.join_next(), if !workers.is_empty() => {
                        let result = result.unwrap_or_else(|error| Err(TunnelError::InternalError(format!("HTTP3 accept task failed: {error}"))));
                        if completed_tx.send(result).await.is_err() { break; }
                    }
                    accepted = endpoint.accept(), if incoming.is_none() => { let Some(accepted) = accepted else { break; }; incoming = Some(accepted); }
                    permit = handshakes.clone().acquire_owned(), if incoming.is_some() => {
                        let Ok(permit) = permit else { break; };
                        match incoming.take().unwrap().accept() {
                            Ok(connecting) => { workers.spawn(finish_http3_tunnel(connecting, endpoint.clone(), local_url.clone(), permit)); }
                            Err(error) => { tracing::debug!(?error, "HTTP3 incoming connection rejected"); }
                        }
                    }
                }
            }
        });
        Ok(Self {
            completed,
            _task: AbortOnDropHandle::new(task),
        })
    }
    pub(crate) async fn accept(&mut self) -> Result<Box<dyn Tunnel>, TunnelError> {
        while let Some(result) = self.completed.recv().await {
            match result {
                Ok(tunnel) => return Ok(tunnel),
                Err(error) => tracing::debug!(?error, "HTTP3 session connection ended"),
            }
        }
        Err(TunnelError::Shutdown)
    }
}
#[async_trait::async_trait]
impl ServerTunnelAcceptor for Http3AcceptedSession {
    async fn accept(&mut self) -> anyhow::Result<Box<dyn Tunnel>> {
        Ok(Http3AcceptedSession::accept(self).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use easytier_core::{
        connectivity::protocol::ServerProtocolAdmissionController, socket::SocketListener,
    };
    use futures::{SinkExt, StreamExt};
    use std::net::SocketAddr;

    #[test]
    fn tls_and_request_policy_preserve_sni_authority_and_explicit_path() {
        assert_eq!(
            build_rustls_client_config().unwrap().alpn_protocols,
            vec![b"h3".to_vec()]
        );
        assert_eq!(
            build_rustls_server_config().unwrap().alpn_protocols,
            vec![b"h3".to_vec()]
        );
        let url = "http3://[::1]:8443?traffic_disguise=video&sni=tls.example&host=front.example:443&path=/segments/live&keep=yes".parse().unwrap();
        assert_eq!(resolve_sni(&url), "tls.example");
        let uri = request_uri(&url, TrafficStyle::from_url(&url)).unwrap();
        assert_eq!(uri.host(), Some("front.example"));
        assert_eq!(uri.authority().unwrap().as_str(), "front.example:443");
        assert_eq!(
            uri.path_and_query().unwrap().as_str(),
            "/segments/live?keep=yes"
        );
        for url in ["http3://[::1]:11014", "http3://[::1]:11014?sni="] {
            assert_eq!(resolve_sni(&url.parse().unwrap()), "::1");
        }
    }

    #[tokio::test]
    async fn http3_endpoints_keep_bbr() {
        super::super::quic::tests::assert_endpoint_controllers(
            client_config().unwrap(),
            server_config().unwrap(),
        )
        .await;
    }

    #[rstest::rstest]
    #[case("127.0.0.1:0")]
    #[case("[::1]:0")]
    #[tokio::test(flavor = "multi_thread")]
    async fn independent_h3_get_and_duplex_post_preserve_split_coalesced_frames(
        #[case] bind: &str,
    ) {
        tokio::time::timeout(Duration::from_secs(15), async {
            let server_endpoint =
                quinn::Endpoint::server(server_config().unwrap(), bind.parse().unwrap()).unwrap();
            let address = server_endpoint.local_addr().unwrap();
            let server_clone = server_endpoint.clone();
            let permits = Arc::new(Semaphore::new(1));
            let server = tokio::spawn(async move {
                let incoming = server_clone.accept().await.unwrap().accept().unwrap();
                finish_http3_tunnel(
                    incoming,
                    server_clone,
                    format!("http3://{address}").parse().unwrap(),
                    permits.acquire_owned().await.unwrap(),
                )
                .await
                .unwrap()
            });
            let any: SocketAddr = if address.is_ipv6() {
                "[::1]:0"
            } else {
                "127.0.0.1:0"
            }
            .parse()
            .unwrap();
            // Quinn's client helper requests dual stack before binding; on
            // Windows this fails for an IPv6 loopback-only bind.
            let socket = std::net::UdpSocket::bind(any).unwrap();
            let mut client_endpoint = quinn::Endpoint::new(
                EndpointConfig::default(),
                None,
                socket,
                quinn::default_runtime().unwrap(),
            )
            .unwrap();
            client_endpoint.set_default_client_config(client_config().unwrap());
            let connection = client_endpoint
                .connect(address, "localhost")
                .unwrap()
                .await
                .unwrap();
            let (mut driver, mut requests) =
                h3::client::new(h3_quinn::Connection::new(connection.clone()))
                    .await
                    .unwrap();
            let driver_task = AbortOnDropHandle::new(tokio::spawn(async move {
                let _ = driver.wait_idle().await;
            }));
            // This is an ordinary h3 client, with no EasyTier transport upgrade.
            let mut get = requests
                .send_request(
                    http::Request::builder()
                        .uri(format!("https://{address}/"))
                        .body(())
                        .unwrap(),
                )
                .await
                .unwrap();
            get.finish().await.unwrap();
            assert_eq!(
                get.recv_response().await.unwrap().status(),
                http::StatusCode::OK
            );
            let mut body = Vec::new();
            while let Some(mut chunk) = get.recv_data().await.unwrap() {
                body.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
            }
            assert_eq!(body, b"OK\n");
            let mut post = requests
                .send_request(
                    http::Request::builder()
                        .method(http::Method::POST)
                        .uri(format!("https://{address}/upload"))
                        .body(())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                post.recv_response().await.unwrap().status(),
                http::StatusCode::OK
            );
            let tunnel = server.await.unwrap();
            let (mut read, mut write) = tunnel.split();
            let first = TcpZCPacketToBytes
                .zcpacket_into_bytes(ZCPacket::new_with_payload(b"split frame"))
                .unwrap();
            let second = TcpZCPacketToBytes
                .zcpacket_into_bytes(ZCPacket::new_with_payload(b"coalesced frame"))
                .unwrap();
            post.send_data(first.slice(..1)).await.unwrap();
            post.send_data(first.slice(1..3)).await.unwrap();
            let mut tail = BytesMut::from(&first[3..]);
            tail.extend_from_slice(&second);
            post.send_data(tail.freeze()).await.unwrap();
            assert_eq!(
                read.next().await.unwrap().unwrap().payload(),
                b"split frame"
            );
            assert_eq!(
                read.next().await.unwrap().unwrap().payload(),
                b"coalesced frame"
            );
            for value in [b"first response".as_slice(), b"second response".as_slice()] {
                write.send(ZCPacket::new_with_payload(value)).await.unwrap();
            }
            write.close().await.unwrap();
            let mut bytes = BytesMut::new();
            let mut received = Vec::new();
            while received.len() < 2 {
                let mut data = post.recv_data().await.unwrap().unwrap();
                bytes.extend_from_slice(&data.copy_to_bytes(data.remaining()));
                while let Some(packet) = FramedReader::<tokio::io::Empty>::extract_one_packet(
                    &mut bytes,
                    session_common::MAX_TUNNEL_BODY_SIZE,
                ) {
                    received.push(packet.unwrap().payload().to_vec());
                }
            }
            assert_eq!(
                received,
                vec![b"first response".to_vec(), b"second response".to_vec()]
            );
            drop(read);
            drop(write);
            drop(tunnel);
            tokio::time::timeout(Duration::from_secs(3), connection.closed())
                .await
                .expect("last tunnel half must release the HTTP3 connection");
            drop(driver_task);
            client_endpoint.close(0u32.into(), b"");
            server_endpoint.close(0u32.into(), b"");
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn classified_udp_session_carries_real_http3_and_frame_boundaries() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (mut listener, connected, _) =
                super::super::quic::session_tests::connected_udp_session_fixture("http3").await;
            let local_url = listener.local_url();
            let remote_url = local_url.clone();
            let server = tokio::spawn(async move {
                let session = listener.accept().await.unwrap();
                let admission = ServerProtocolAdmissionController::quic()
                    .try_admit()
                    .unwrap();
                let mut accepted =
                    Http3AcceptedSession::new(session, local_url, admission).unwrap();
                accepted.accept().await.unwrap()
            });
            let client = upgrade_connected(connected, remote_url).await.unwrap();
            let server = server.await.unwrap();
            let (mut client_read, mut client_write) = client.split();
            let (mut server_read, mut server_write) = server.split();
            for size in [
                1,
                2001,
                session_common::MAX_TUNNEL_BODY_SIZE - PEER_MANAGER_HEADER_SIZE,
            ] {
                let payload = vec![0x41; size];
                client_write
                    .send(ZCPacket::new_with_payload(&payload))
                    .await
                    .unwrap();
                assert_eq!(
                    server_read.next().await.unwrap().unwrap().payload(),
                    payload
                );
                server_write
                    .send(ZCPacket::new_with_payload(&payload))
                    .await
                    .unwrap();
                assert_eq!(
                    client_read.next().await.unwrap().unwrap().payload(),
                    payload
                );
            }
            let oversized = ZCPacket::new_with_payload(&vec![
                0;
                session_common::MAX_TUNNEL_BODY_SIZE
                    + 1
                    - PEER_MANAGER_HEADER_SIZE
            ]);
            assert!(client_write.send(oversized).await.is_err());
        })
        .await
        .unwrap();
    }

    #[test]
    fn outgoing_frame_limit_is_checked_before_allocation() {
        let max_payload = session_common::MAX_TUNNEL_BODY_SIZE - PEER_MANAGER_HEADER_SIZE;
        assert!(
            CheckedFrameConverter
                .zcpacket_into_bytes(ZCPacket::new_with_payload(&vec![0; max_payload]))
                .is_ok()
        );
        assert!(
            CheckedFrameConverter
                .zcpacket_into_bytes(ZCPacket::new_with_payload(&vec![0; max_payload + 1]))
                .is_err()
        );
        for kind in [PacketType::ParallelData, PacketType::ParallelLossyData] {
            let mut parallel =
                ZCPacket::new_with_payload(&vec![0; max_payload + PARALLEL_FRAME_OVERHEAD]);
            parallel.fill_peer_manager_hdr(1, 2, kind as u8);
            assert!(CheckedFrameConverter.zcpacket_into_bytes(parallel).is_ok());
            let mut oversized =
                ZCPacket::new_with_payload(&vec![0; max_payload + PARALLEL_FRAME_OVERHEAD + 1]);
            oversized.fill_peer_manager_hdr(1, 2, kind as u8);
            assert!(
                CheckedFrameConverter
                    .zcpacket_into_bytes(oversized)
                    .is_err()
            );
        }
    }
}
