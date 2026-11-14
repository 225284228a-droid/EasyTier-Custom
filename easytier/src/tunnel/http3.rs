//! HTTP/3 duplex streaming POST tunnels; control streams and QPACK use h3.
pub(crate) use super::quic::QuicUdpSessionSocket as Http3UdpSessionSocket;
pub(crate) use super::quic::fec_session::FecConfig;
use super::{
    insecure_tls::{SkipServerVerification, get_insecure_tls_cert, sni_from_url},
    quic::{
        fec_session::{FecHandle, FecUdpSession},
        session_common, window_source,
    },
    traffic_disguise::{PacedSink, TrafficStyle},
};
use crate::proto::common::TunnelInfo;
use anyhow::Context as _;
use bytes::{Buf, Bytes};
use easytier_core::{
    connectivity::{
        protocol::{ServerProtocolAdmission, ServerTunnelAcceptor},
        transport::ConnectedUdpSession,
    },
    packet::{PEER_MANAGER_HEADER_SIZE, PacketType, ZCPacket},
    socket::udp::UdpSession,
    tunnel::{
        Tunnel, TunnelError, flow_lanes,
        framed::{FramedReader, FramedWriter, TcpZCPacketToBytes, ZCPacketToBytes},
        wrapper::TunnelWrapper,
    },
};
use futures::{Sink, StreamExt as _};
use quinn::{
    AsyncUdpSocket, ClientConfig, EndpointConfig, ServerConfig, TransportConfig,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
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
const PARALLEL_FRAME_OVERHEAD: usize = easytier_core::packet::PARALLEL_PACKET_OVERHEAD;
const MAX_HTTP3_FRAME_SIZE: usize = session_common::MAX_TUNNEL_BODY_SIZE + PARALLEL_FRAME_OVERHEAD;
type H3ServerConnection = h3::server::Connection<h3_quinn::Connection, Bytes>;
type H3ServerStream = h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

/// Carries the FEC key inside TLS. A request identifier that the response
/// echoes is ordinary HTTP; here the echo also says the server took the key.
const FEC_HEADER: &str = "x-request-id";
/// QUIC declares a packet lost once this many later packets are acknowledged,
/// or once it is 9/8 RTT old. Below QUIC, FEC rebuilds a lost datagram a few
/// datagrams later; declaring it lost first would retransmit it needlessly
/// and, worse, put BBR into recovery for a loss that never reached QUIC. A
/// count above the widest repair span leaves detection to the time rule: a
/// loss FEC cannot mend is found at most RTT/8 later than with the default.
///
/// The count is part of a connection's transport configuration, fixed before
/// the handshake, while FEC is agreed in the request after it. Each tunnel
/// therefore uses it when its own end asks for FEC; an end that only answers
/// keeps QUIC's default, so its direction may still retransmit a datagram
/// that a repair would have mended.
const FEC_PACKET_THRESHOLD: u32 = 40;
/// The client asks for this many request streams on the first POST; the
/// answer carries the number both ends allow. Every further POST names its
/// lane. All of them are ordinary concurrent uploads on one connection.
const STREAMS_HEADER: &str = "x-upload-streams";
const LANE_HEADER: &str = "x-upload-part";
/// Packets queued for lanes whose stream cannot take more yet.
const LANE_QUEUE_LIMIT: usize = 256;

/// What the local configuration asks of an HTTP3 tunnel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Http3Options {
    pub fec: FecConfig,
    /// Reliable streams to spread flows over; see [`flow_lanes`].
    pub streams: usize,
}

impl Http3Options {
    pub(crate) const DEFAULT_STREAMS: usize = 8;

    /// One stream and no FEC, the tunnel as it was before either existed.
    #[cfg(test)]
    pub(crate) const PLAIN: Self = Self {
        fec: FecConfig::OFF,
        streams: 1,
    };

    pub(crate) fn from_flags(flags: &crate::proto::common::Flags) -> Self {
        Self {
            fec: FecConfig::from_flags(flags.enable_udp_fec, flags.http3_fec_ratio),
            streams: (flags.http3_streams as usize).clamp(1, flow_lanes::MAX_LANES),
        }
    }

    /// `fec=` and `streams=` on a tunnel URL override the instance setting.
    pub(crate) fn with_url_override(self, url: &url::Url) -> Self {
        let streams = url
            .query_pairs()
            .find(|(key, _)| key == "streams")
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .map_or(self.streams, |streams| {
                streams.clamp(1, flow_lanes::MAX_LANES)
            });
        Self {
            fec: self.fec.with_url_override(url),
            streams,
        }
    }
}

fn parse_count(value: &http::HeaderValue) -> Option<usize> {
    value
        .to_str()
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|count| (1..=flow_lanes::MAX_LANES).contains(count))
}

/// A fresh key as a header value; its last bit says whether this end wants
/// protection, since the peer may want it even when this end does not.
fn fec_offer(wanted: bool) -> ([u8; 16], String) {
    let mut key: [u8; 16] = rand::random();
    key[15] = (key[15] & !1) | u8::from(wanted);
    let text = key.iter().map(|byte| format!("{byte:02x}")).collect();
    (key, text)
}

fn parse_fec_offer(value: &http::HeaderValue) -> Option<([u8; 16], bool)> {
    let text = value.to_str().ok()?;
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    let mut key = [0; 16];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some((key, key[15] & 1 == 1))
}

pub fn transport_config() -> Arc<TransportConfig> {
    transport_config_for(false)
}
fn transport_config_for(fec: bool) -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    if fec {
        config.packet_threshold(FEC_PACKET_THRESHOLD);
    }
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
    server_config_for(false)
}
pub(crate) fn server_config_for(fec: bool) -> anyhow::Result<ServerConfig> {
    let mut config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(
        build_rustls_server_config()?,
    )?));
    config.transport_config(transport_config_for(fec));
    Ok(config)
}
pub fn client_config() -> anyhow::Result<ClientConfig> {
    client_config_for(false)
}
pub(crate) fn client_config_for(fec: bool) -> anyhow::Result<ClientConfig> {
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(
        build_rustls_client_config()?,
    )?));
    config.transport_config(transport_config_for(fec));
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
                "sni"
                    | "host"
                    | "path"
                    | "ua"
                    | "accept_language"
                    | "padding"
                    | "traffic_disguise"
                    | "fec"
                    | "streams"
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
    connection: super::quic::ConnWrapper,
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
type LaneWriter = FramedWriter<tokio::io::DuplexStream, CheckedFrameConverter>;
type LaneReader = Pin<Box<dyn futures::Stream<Item = Result<ZCPacket, TunnelError>> + Send>>;

/// Sends each packet on the lane [`flow_lanes::lane_of`] picks. A lane whose
/// stream cannot take more holds only its own packets back, up to a shared
/// limit; QUIC streams stall on send only when the peer's window for that
/// stream is exhausted.
struct LaneSink {
    lanes: Vec<LaneWriter>,
    queues: Vec<VecDeque<ZCPacket>>,
    queued: usize,
}

impl LaneSink {
    fn new(lanes: Vec<LaneWriter>) -> Self {
        Self {
            queues: lanes.iter().map(|_| VecDeque::new()).collect(),
            lanes,
            queued: 0,
        }
    }

    /// Moves queued packets into their lanes; ready once every queue is empty.
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TunnelError>> {
        let mut idle = true;
        for (lane, queue) in self.lanes.iter_mut().zip(&mut self.queues) {
            while !queue.is_empty() {
                match Pin::new(&mut *lane).poll_ready(cx) {
                    Poll::Ready(Ok(())) => {
                        Pin::new(&mut *lane).start_send(queue.pop_front().unwrap())?;
                        self.queued -= 1;
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => {
                        idle = false;
                        break;
                    }
                }
            }
        }
        if idle {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl Sink<ZCPacket> for LaneSink {
    type Error = TunnelError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if let Poll::Ready(Err(error)) = self.drain(cx) {
            return Poll::Ready(Err(error));
        }
        if self.queued < LANE_QUEUE_LIMIT {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn start_send(mut self: Pin<&mut Self>, packet: ZCPacket) -> Result<(), Self::Error> {
        let lane = flow_lanes::lane_of(&packet, self.lanes.len());
        self.queues[lane].push_back(packet);
        self.queued += 1;
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        futures::ready!(self.drain(cx))?;
        for lane in &mut self.lanes {
            futures::ready!(Pin::new(lane).poll_flush(cx))?;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        futures::ready!(self.drain(cx))?;
        for lane in &mut self.lanes {
            futures::ready!(Pin::new(lane).poll_close(cx))?;
        }
        Poll::Ready(Ok(()))
    }
}

/// One request stream of a tunnel: its body carries framed packets in both
/// directions, and each lane is ordered only within itself.
fn body_lane(
    mut send: impl BodySend,
    mut recv: impl BodyRecv,
    lifetime: &Arc<H3Lifetime>,
) -> (LaneWriter, LaneReader) {
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
    let reader = FramedReader::new_with_associate_data(
        read,
        MAX_HTTP3_FRAME_SIZE,
        Some(Box::new(lifetime.clone())),
    )
    .map(|result| {
        result.and_then(|packet| {
            validate_frame(&packet)?;
            Ok(packet)
        })
    });
    let writer = FramedWriter::new_with_converter_and_associate_data(
        write,
        CheckedFrameConverter,
        Some(Box::new(lifetime.clone())),
    );
    (writer, Box::pin(reader))
}

fn body_tunnel<S: BodySend, R: BodyRecv>(
    lanes: Vec<(S, R)>,
    lifetime: Arc<H3Lifetime>,
    style: TrafficStyle,
    info: TunnelInfo,
) -> Box<dyn Tunnel> {
    let (writers, readers): (Vec<_>, Vec<_>) = lanes
        .into_iter()
        .map(|(send, recv)| body_lane(send, recv, &lifetime))
        .unzip();
    let bandwidth_source = window_source(&lifetime.connection.conn);
    // Lanes keep the connection alive through their readers and writers.
    drop(lifetime);
    Box::new(
        TunnelWrapper::new(
            futures::stream::select_all(readers),
            PacedSink::new(LaneSink::new(writers), style),
            Some(info),
        )
        .with_bandwidth_source(Some(bandwidth_source)),
    )
}
#[cfg(test)]
pub(crate) async fn upgrade_connected(
    connected: ConnectedUdpSession,
    remote_url: url::Url,
) -> Result<Box<dyn Tunnel>, TunnelError> {
    upgrade_connected_with(connected, remote_url, Http3Options::PLAIN).await
}
pub(crate) async fn upgrade_connected_with(
    connected: ConnectedUdpSession,
    remote_url: url::Url,
    options: Http3Options,
) -> Result<Box<dyn Tunnel>, TunnelError> {
    let (session, session_guard) = connected.into_parts();
    Ok(
        connect_tunnel(Arc::new(session), session_guard, remote_url, options)
            .await?
            .tunnel,
    )
}
/// A connected client tunnel with what tests inspect of it.
#[cfg_attr(not(test), allow(dead_code))]
struct ClientTunnel {
    tunnel: Box<dyn Tunnel>,
    fec: FecHandle,
    lanes: usize,
}
async fn connect_tunnel(
    session: Arc<dyn easytier_core::socket::udp::UdpSessionSocket>,
    session_guard: Box<dyn Send + Sync>,
    remote_url: url::Url,
    options: Http3Options,
) -> Result<ClientTunnel, TunnelError> {
    let options = options.with_url_override(&remote_url);
    let fec = options.fec;
    let style = TrafficStyle::from_url(&remote_url);
    let (session, fec_handle) = FecUdpSession::new(session);
    let socket = Arc::new(Http3UdpSessionSocket::from_session(session, session_guard)?);
    let local_addr = socket.local_addr()?;
    let remote_addr = socket.peer_addr();
    let mut endpoint = session_common::build_session_endpoint(socket, None, endpoint_config())?;
    endpoint.set_default_client_config(client_config_for(fec.wanted)?);
    let connecting = endpoint
        .connect(remote_addr, &resolve_sni(&remote_url))
        .map_err(anyhow::Error::new)?;
    let connection = tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
        .await
        .map_err(TunnelError::Timeout)?
        .context("HTTP3 TLS connection failed")?;
    let resolved_remote_addr = connection.remote_address();
    let lifetime = Arc::new(H3Lifetime {
        connection: super::quic::ConnWrapper::new(connection.clone(), endpoint),
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
    let uri = request_uri(&remote_url, style)?;
    let new_request = |extra: &[(&str, String)]| {
        let mut request = http::Request::builder()
            .method(http::Method::POST)
            .uri(uri.clone())
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
        for (name, value) in extra {
            request = request.header(*name, value.as_str());
        }
        request.body(()).map_err(anyhow::Error::new)
    };
    // Understand protected datagrams before the server can send any.
    let (fec_key, fec_offer) = fec_offer(fec.wanted);
    fec_handle.rekey(fec_key);
    let request = new_request(&[
        (FEC_HEADER, fec_offer.clone()),
        (STREAMS_HEADER, options.streams.to_string()),
    ])?;
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
    if response
        .headers()
        .get(FEC_HEADER)
        .is_some_and(|echo| echo.as_bytes() == fec_offer.as_bytes())
    {
        fec_handle.enable_tx(fec.mode);
    } else {
        fec_handle.disarm();
    }
    // A server that does not answer the stream count keeps one stream.
    let lanes = response
        .headers()
        .get(STREAMS_HEADER)
        .and_then(parse_count)
        .map_or(1, |lanes| lanes.min(options.streams));
    let extra = (1..lanes).map(|lane| {
        let request = new_request(&[(LANE_HEADER, lane.to_string())]);
        let mut requests = requests.clone();
        async move {
            let mut stream = requests
                .send_request(request?)
                .await
                .context("HTTP3 POST failed")?;
            let response = stream
                .recv_response()
                .await
                .context("HTTP3 response failed")?;
            anyhow::ensure!(
                response.status().is_success(),
                "HTTP3 POST rejected: {}",
                response.status()
            );
            Ok::<_, anyhow::Error>(stream.split())
        }
    });
    let extra = tokio::time::timeout(HANDSHAKE_TIMEOUT, futures::future::try_join_all(extra))
        .await
        .map_err(TunnelError::Timeout)??;
    let mut streams = vec![stream.split()];
    streams.extend(extra);
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
    Ok(ClientTunnel {
        tunnel: body_tunnel(streams, lifetime, style, info),
        fec: fec_handle,
        lanes,
    })
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
    (fec_handle, options): (FecHandle, Http3Options),
    _permit: OwnedSemaphorePermit,
) -> Result<Box<dyn Tunnel>, TunnelError> {
    let fec = options.fec;
    let connection = tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
        .await
        .map_err(TunnelError::Timeout)?
        .context("HTTP3 incoming TLS connection failed")?;
    let remote_addr = connection.remote_address();
    let lifetime = Arc::new(H3Lifetime {
        connection: super::quic::ConnWrapper::new(connection.clone(), endpoint),
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
    let (request, mut stream) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        loop {
            let resolver = driver
                .accept()
                .await?
                .ok_or_else(|| anyhow::anyhow!("HTTP3 connection closed before POST"))?;
            let (request, mut stream) = resolver.resolve_request().await?;
            if request.method() == http::Method::POST {
                break Ok::<_, anyhow::Error>((request, stream));
            }
            ordinary_response(&request, &mut stream).await?;
        }
    })
    .await
    .map_err(TunnelError::Timeout)??;
    let mut response = http::Response::builder()
        .status(200)
        .header(http::header::CONTENT_TYPE, "application/octet-stream")
        .header(http::header::CACHE_CONTROL, "no-store");
    // Either end asking protects both directions. The client armed itself
    // before sending the key, so this end may protect the response already.
    //
    // A UDP session carries one client endpoint, and that endpoint makes one
    // connection. A second POST on the session is therefore a client that
    // reconnected from the same address: its key, or its lack of one, replaces
    // the old connection's, which nothing can read any more.
    match request
        .headers()
        .get(FEC_HEADER)
        .and_then(|offer| Some((offer, parse_fec_offer(offer)?)))
    {
        Some((offer, (key, client_wants))) if client_wants || fec.wanted => {
            fec_handle.rekey(key);
            fec_handle.enable_tx(fec.mode);
            response = response.header(FEC_HEADER, offer);
        }
        _ => fec_handle.disarm(),
    }
    // A client that asks for no stream count is answered with none.
    let lanes = match request.headers().get(STREAMS_HEADER) {
        Some(requested) => {
            let lanes = parse_count(requested).unwrap_or(1).min(options.streams);
            response = response.header(STREAMS_HEADER, lanes.to_string());
            lanes
        }
        None => 1,
    };
    stream
        .send_response(response.body(()).map_err(anyhow::Error::new)?)
        .await
        .context("HTTP3 POST response failed")?;
    let mut lane_streams: Vec<Option<H3ServerStream>> = (1..lanes).map(|_| None).collect();
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        while lane_streams.iter().any(Option::is_none) {
            let resolver = driver
                .accept()
                .await?
                .ok_or_else(|| anyhow::anyhow!("HTTP3 connection closed before its streams"))?;
            let (request, mut lane_stream) = resolver.resolve_request().await?;
            let slot = request
                .headers()
                .get(LANE_HEADER)
                .and_then(parse_count)
                .filter(|_| request.method() == http::Method::POST)
                .and_then(|lane| lane_streams.get_mut(lane.checked_sub(1)?))
                .filter(|slot| slot.is_none());
            let Some(slot) = slot else {
                ordinary_response(&request, &mut lane_stream).await?;
                continue;
            };
            lane_stream
                .send_response(
                    http::Response::builder()
                        .status(200)
                        .header(http::header::CONTENT_TYPE, "application/octet-stream")
                        .header(http::header::CACHE_CONTROL, "no-store")
                        .body(())?,
                )
                .await
                .context("HTTP3 POST response failed")?;
            *slot = Some(lane_stream);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(TunnelError::Timeout)??;
    let streams = std::iter::once(stream)
        .chain(lane_streams.into_iter().flatten())
        .map(|stream| stream.split())
        .collect();
    lifetime.spawn(serve_remaining_requests(driver));
    let style = TrafficStyle::from_url(&local_url);
    let remote_url = super::build_url_from_socket_addr(&remote_addr.to_string(), "http3");
    let info = TunnelInfo {
        tunnel_type: "http3".to_owned(),
        local_addr: Some(local_url.into()),
        remote_addr: Some(remote_url.clone().into()),
        resolved_remote_addr: Some(remote_url.into()),
    };
    Ok(body_tunnel(streams, lifetime, style, info))
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
        options: Http3Options,
    ) -> Result<Self, TunnelError> {
        let (active_session, handshakes) = admission.into_parts();
        Self::on_session(
            Arc::new(session),
            Box::new(active_session),
            local_url,
            handshakes,
            options,
        )
    }
    fn on_session(
        session: Arc<dyn easytier_core::socket::udp::UdpSessionSocket>,
        session_guard: Box<dyn Send + Sync>,
        local_url: url::Url,
        handshakes: Arc<Semaphore>,
        options: Http3Options,
    ) -> Result<Self, TunnelError> {
        let options = options.with_url_override(&local_url);
        let (session, fec_handle) = FecUdpSession::new(session);
        let socket = Arc::new(Http3UdpSessionSocket::from_session(session, session_guard)?);
        let endpoint = session_common::build_session_endpoint(
            socket,
            Some(server_config_for(options.fec.wanted)?),
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
                            Ok(connecting) => { workers.spawn(finish_http3_tunnel(connecting, endpoint.clone(), local_url.clone(), (fec_handle.clone(), options), permit)); }
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
        super::super::quic::session_tests::assert_endpoint_controllers(
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
                    (FecHandle::detached(), Http3Options::PLAIN),
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
                    Http3AcceptedSession::new(session, local_url, admission, Http3Options::PLAIN)
                        .unwrap();
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

    /// Runs one tunnel with the given FEC settings, passes traffic both ways
    /// and returns the client's view of the session's FEC.
    async fn fec_round_trip(
        client: FecConfig,
        server: FecConfig,
        query: &str,
    ) -> super::super::quic::fec_session::FecStats {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (mut listener, connected, _) =
                super::super::quic::session_tests::connected_udp_session_fixture("http3").await;
            let local_url = listener.local_url();
            let mut remote_url = local_url.clone();
            if !query.is_empty() {
                remote_url.set_query(Some(query));
            }
            let server = tokio::spawn(async move {
                let session = listener.accept().await.unwrap();
                let admission = ServerProtocolAdmissionController::quic()
                    .try_admit()
                    .unwrap();
                let options = Http3Options {
                    fec: server,
                    streams: 1,
                };
                let mut accepted =
                    Http3AcceptedSession::new(session, local_url, admission, options).unwrap();
                accepted.accept().await.unwrap()
            });
            let (session, guard) = connected.into_parts();
            let options = Http3Options {
                fec: client,
                streams: 1,
            };
            let ClientTunnel {
                tunnel: client,
                fec: handle,
                ..
            } = connect_tunnel(Arc::new(session), guard, remote_url, options)
                .await
                .unwrap();
            let server = server.await.unwrap();
            let (mut client_read, mut client_write) = client.split();
            let (mut server_read, mut server_write) = server.split();
            for round in 0..40 {
                let payload = vec![round as u8; 900];
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
            handle.stats()
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn either_end_asking_protects_both_directions_of_an_http3_tunnel() {
        use super::super::quic::datagram_fec::RatioMode;
        let fixed = FecConfig {
            wanted: true,
            mode: RatioMode::Fixed(0.5),
        };
        for (client, server, query, active) in [
            (FecConfig::OFF, FecConfig::OFF, "", false),
            (fixed, FecConfig::OFF, "", true),
            (FecConfig::OFF, fixed, "", true),
            (fixed, fixed, "", true),
            (fixed, FecConfig::OFF, "fec=off", false),
            (FecConfig::OFF, FecConfig::OFF, "fec=50", true),
        ] {
            let stats = fec_round_trip(client, server, query).await;
            assert_eq!(stats.active, active, "{client:?} {server:?} {query}");
            if active {
                // Both ends send sources. A fixed 50% end sends a repair per
                // two sources; an end that only answered adapts, which means
                // the 5% floor until its first measurement.
                assert!(stats.tx_sources >= 40, "{stats:?}");
                assert!(stats.rx_sources >= 40, "{stats:?}");
                assert_eq!(stats.recovered, 0, "{stats:?}");
                let client_fixed = client.wanted || !query.is_empty();
                assert_eq!(stats.tx_repairs >= 15, client_fixed, "{stats:?}");
                assert_eq!(stats.rx_repairs >= 15, server.wanted, "{stats:?}");
            } else {
                assert_eq!((stats.tx_sources, stats.rx_sources), (0, 0), "{stats:?}");
            }
        }
    }

    /// An unencrypted UDP data packet of flow `port` carrying `stamp`.
    fn flow_packet(port: u16, len: usize, stamp: u64) -> ZCPacket {
        let total = 28 + len.max(8);
        let mut bytes = vec![0; total];
        bytes[0] = 0x45;
        bytes[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        bytes[9] = 17;
        bytes[12..16].copy_from_slice(&[10, 0, 0, 1]);
        bytes[16..20].copy_from_slice(&[10, 0, 0, 2]);
        bytes[20..22].copy_from_slice(&port.to_be_bytes());
        bytes[22..24].copy_from_slice(&9u16.to_be_bytes());
        bytes[28..36].copy_from_slice(&stamp.to_le_bytes());
        let mut packet = ZCPacket::new_with_payload(&bytes);
        packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        flow_lanes::mark_flow(&mut packet);
        packet
    }

    fn flow_of(packet: &ZCPacket) -> (u16, u64) {
        let payload = packet.payload();
        (
            u16::from_be_bytes(payload[20..22].try_into().unwrap()),
            u64::from_le_bytes(payload[28..36].try_into().unwrap()),
        )
    }

    fn options(streams: usize) -> Http3Options {
        Http3Options {
            fec: FecConfig::OFF,
            streams,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lanes_follow_the_smaller_end_and_keep_each_flow_in_order() {
        for (client_streams, server_streams, query, expected) in [
            (8, 3, "", 3),
            (1, 8, "", 1),
            (8, 1, "", 1),
            (8, 8, "streams=2", 2),
            (5, 5, "", 5),
        ] {
            tokio::time::timeout(Duration::from_secs(15), async {
                let (mut listener, connected, _) =
                    super::super::quic::session_tests::connected_udp_session_fixture("http3").await;
                let local_url = listener.local_url();
                let mut remote_url = local_url.clone();
                if !query.is_empty() {
                    remote_url.set_query(Some(query));
                }
                let server = tokio::spawn(async move {
                    let session = listener.accept().await.unwrap();
                    let admission = ServerProtocolAdmissionController::quic()
                        .try_admit()
                        .unwrap();
                    let mut accepted = Http3AcceptedSession::new(
                        session,
                        local_url,
                        admission,
                        options(server_streams),
                    )
                    .unwrap();
                    accepted.accept().await.unwrap()
                });
                let (session, guard) = connected.into_parts();
                let client = connect_tunnel(
                    Arc::new(session),
                    guard,
                    remote_url,
                    options(client_streams),
                )
                .await
                .unwrap();
                assert_eq!(
                    client.lanes, expected,
                    "{client_streams} {server_streams} {query}"
                );
                let server = server.await.unwrap();
                let (_client_read, mut client_write) = client.tunnel.split();
                let (mut server_read, _server_write) = server.split();
                let total = 600u64;
                for index in 0..total {
                    client_write
                        .feed(flow_packet(1000 + (index % 30) as u16, 300, index))
                        .await
                        .unwrap();
                }
                client_write.flush().await.unwrap();
                let mut last = std::collections::HashMap::new();
                for _ in 0..total {
                    let packet = server_read.next().await.unwrap().unwrap();
                    let (port, index) = flow_of(&packet);
                    if let Some(previous) = last.insert(port, index) {
                        assert!(previous < index, "flow {port} reordered");
                    }
                }
                assert_eq!(last.len(), 30);
            })
            .await
            .unwrap();
        }
    }

    struct ProbeLatency {
        median: Duration,
        p90: Duration,
        max: Duration,
    }

    /// Sends bulk traffic on several flows and a sparse probe flow on another
    /// stream over a 20 ms RTT path that loses `loss` of the client's
    /// datagrams, and measures how long the probe's packets take.
    async fn probe_latency(streams: usize, loss: f64) -> ProbeLatency {
        use super::super::quic::test_link;
        let (client_link, server_link) = test_link::pair(loss, Duration::from_millis(10));
        let url: url::Url = format!("http3://{}", test_link::SERVER).parse().unwrap();
        let mut accepted = Http3AcceptedSession::on_session(
            server_link,
            Box::new(()),
            url.clone(),
            Arc::new(Semaphore::new(4)),
            options(streams),
        )
        .unwrap();
        let server = tokio::spawn(async move {
            let tunnel = accepted.accept().await.unwrap();
            (tunnel, accepted)
        });
        let client = connect_tunnel(client_link, Box::new(()), url, options(streams))
            .await
            .unwrap();
        assert_eq!(client.lanes, streams);
        let (server, _accepted) = server.await.unwrap();
        let (_client_read, mut client_write) = client.tunnel.split();
        let (mut server_read, _server_write) = server.split();

        // The probe has a stream to itself whenever there is more than one.
        let probe = 1000u16;
        let probe_lane = flow_lanes::lane_of(&flow_packet(probe, 100, 0), 8);
        let bulk: Vec<u16> = (2000..)
            .filter(|port| flow_lanes::lane_of(&flow_packet(*port, 100, 0), 8) != probe_lane)
            .take(6)
            .collect();
        let start = std::time::Instant::now();
        let receiver = tokio::spawn(async move {
            let mut latencies = Vec::new();
            while let Some(Ok(packet)) = server_read.next().await {
                let (port, stamp) = flow_of(&packet);
                if port == probe {
                    let sent = Duration::from_micros(stamp);
                    latencies.push(start.elapsed().saturating_sub(sent));
                    if latencies.len() == 300 {
                        break;
                    }
                }
            }
            latencies
        });
        let mut tick = tokio::time::interval(Duration::from_millis(1));
        for round in 0..1500u64 {
            tick.tick().await;
            let stamp = start.elapsed().as_micros() as u64;
            for slot in 0..3 {
                let port = bulk[(round as usize * 3 + slot) % bulk.len()];
                client_write
                    .feed(flow_packet(port, 1000, stamp))
                    .await
                    .unwrap();
            }
            if round % 5 == 0 {
                client_write
                    .feed(flow_packet(probe, 100, stamp))
                    .await
                    .unwrap();
            }
            client_write.flush().await.unwrap();
        }
        let mut latencies = tokio::time::timeout(Duration::from_secs(10), receiver)
            .await
            .unwrap()
            .unwrap();
        latencies.sort();
        ProbeLatency {
            median: latencies[latencies.len() / 2],
            p90: latencies[latencies.len() * 9 / 10],
            max: *latencies.last().unwrap(),
        }
    }

    /// A timing comparison: run it alone, since a saturated CPU makes the
    /// in-memory path slower than its losses. Deterministic coverage of the
    /// same isolation is in the stripe receiver and lane selection tests.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "timing benchmark; run alone with --ignored"]
    async fn a_loss_holds_back_only_the_flows_on_its_stream() {
        let one = probe_latency(1, 0.03).await;
        let many = probe_latency(8, 0.03).await;
        for (name, latency) in [("1 stream", &one), ("8 streams", &many)] {
            eprintln!(
                "{name}: probe median {:?}, p90 {:?}, max {:?}",
                latency.median, latency.p90, latency.max
            );
        }
        // On one stream the probe waits behind every lost bulk packet; on its
        // own stream mostly its own losses delay it. QUIC still packs frames
        // of several streams into one datagram, so the separation is partial.
        // The tail depends on scheduling under load; the median does not.
        assert!(many.median * 3 < one.median * 2);
    }

    #[test]
    fn fec_offers_round_trip_and_reject_anything_else() {
        for wanted in [false, true] {
            let (key, text) = fec_offer(wanted);
            assert_eq!(
                parse_fec_offer(&http::HeaderValue::from_str(&text).unwrap()),
                Some((key, wanted))
            );
        }
        for value in ["", "abc", &"g".repeat(32), &"0".repeat(31), &"0".repeat(33)] {
            assert_eq!(
                parse_fec_offer(&http::HeaderValue::from_str(value).unwrap()),
                None
            );
        }
        // The identifier is not a tunnel parameter the server would forward.
        let uri = request_uri(
            &"http3://host.example:443?fec=30&keep=1".parse().unwrap(),
            TrafficStyle::Speedtest,
        )
        .unwrap();
        assert_eq!(uri.path_and_query().unwrap().as_str(), "/api/upload?keep=1");
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
