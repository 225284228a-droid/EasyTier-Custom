use super::FromUrl;
use super::insecure_tls::{
    get_insecure_tls_cert, get_insecure_tls_client_config, init_crypto_provider,
};
use crate::tunnel::common::bind;
use crate::{proto::common::TunnelInfo, socket::tcp::RuntimeTcpSocket};
use anyhow::Context as _;
use bytes::BytesMut;
use cidr::IpCidr;
use easytier_core::{
    packet::{ZCPacket, ZCPacketType},
    socket::tcp::VirtualTcpSocket,
    tunnel::{IpVersion, Tunnel, TunnelError, wrapper::TunnelWrapper},
};
use forwarded_header_value::ForwardedHeaderValue;
use futures::{Sink, StreamExt};
use std::{
    collections::VecDeque,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, LazyLock},
    task::{Context, Poll, ready},
    time::Duration,
};
use tokio::{net::TcpListener, time::timeout};
use tokio_rustls::TlsAcceptor;
use tokio_util::either::Either;
use tokio_websockets::{ClientBuilder, Limits, MaybeTlsStream, Message, ServerBuilder};
use zerocopy::AsBytes as _;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const SERVER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

static TRUSTED_PROXIES: LazyLock<Vec<IpCidr>> = LazyLock::new(|| {
    [
        "127.0.0.0/8",
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "::1/128",
        "fc00::/7",
    ]
    .into_iter()
    .map(|cidr| cidr.parse().unwrap())
    .collect()
});

fn trusted_proxy_contains(ip: IpAddr) -> bool {
    TRUSTED_PROXIES.iter().any(|cidr| match (cidr, ip) {
        (IpCidr::V4(cidr), IpAddr::V4(ip)) => cidr.contains(&ip),
        (IpCidr::V6(cidr), IpAddr::V6(ip)) => cidr.contains(&ip),
        _ => false,
    })
}

fn websocket_error(error: impl std::fmt::Display) -> TunnelError {
    TunnelError::ProtocolError(format!("websocket error: {error}"))
}

fn is_wss(url: &url::Url) -> Result<bool, TunnelError> {
    match url.scheme() {
        "ws" => Ok(false),
        "wss" => Ok(true),
        scheme => Err(TunnelError::InvalidProtocol(scheme.to_owned())),
    }
}

const DEFAULT_BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const DEFAULT_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9,en;q=0.8";
/// Padding frames are carried as WebSocket **text** messages so they can never
/// collide with real ZCPacket payloads, which are always binary. A real
/// ZCPacket over a ws/wss tunnel starts with its `PeerManagerHeader`, whose
/// first byte is the low byte of `from_peer_id` (little-endian) and can legally
/// be `0x00`, so no in-band binary marker could disambiguate padding from real
/// traffic. Text frames are out-of-band at the WebSocket layer and are simply
/// dropped on receive.
const DISGUISE_QUERY_KEYS: [&str; 6] = ["sni", "host", "path", "ua", "accept_language", "padding"];

/// Hard bounds for the `padding=<interval_ms>,<size>` query parameter. The
/// URL is attacker-controllable (it can originate from a peer-broadcast
/// listener), so both values must be clamped before use: an unbounded size
/// would drive periodic oversized allocations and base64 encodings, and a
/// near-zero interval would flood the link with padding frames.
const MIN_PADDING_INTERVAL: Duration = Duration::from_millis(100);
const MAX_PADDING_INTERVAL: Duration = Duration::from_secs(60);
const MAX_PADDING_SIZE: usize = 4096;
const DEFAULT_PADDING_SIZE: usize = 128;

/// Obfuscation parameters for ws/wss tunnels, parsed from URL query params:
/// - `sni`: TLS SNI (and usually the Host header) presented to middleboxes
/// - `host`: Host header value, defaults to the SNI or the URL host
/// - `path`: request path, e.g. `/api/v1/query` to mimic an API endpoint
/// - `ua`: User-Agent header, defaults to a Chrome UA
/// - `accept_language`: Accept-Language header
/// - `padding=<interval_ms>[,<size>]`: send periodic binary padding frames so
///   the flow looks like an interactive HTTPS session instead of a steady pipe
#[derive(Debug, Clone, Default)]
pub struct WssDisguise {
    enabled: bool,
    pub sni: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub user_agent: Option<String>,
    pub accept_language: Option<String>,
    pub padding_interval: Option<Duration>,
    pub padding_size: usize,
}

impl WssDisguise {
    pub fn from_url(url: &url::Url) -> Self {
        let mut disguise = WssDisguise {
            padding_size: DEFAULT_PADDING_SIZE,
            ..Default::default()
        };
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "sni" => {
                    disguise.enabled = true;
                    disguise.sni = Some(value.into_owned());
                }
                "host" => {
                    disguise.enabled = true;
                    disguise.host = Some(value.into_owned());
                }
                "path" => {
                    disguise.enabled = true;
                    disguise.path = Some(value.into_owned());
                }
                "ua" => {
                    disguise.enabled = true;
                    disguise.user_agent = Some(value.into_owned());
                }
                "accept_language" => {
                    disguise.enabled = true;
                    disguise.accept_language = Some(value.into_owned());
                }
                "padding" => {
                    disguise.enabled = true;
                    let mut parts = value.split(',');
                    let interval_ms = parts.next().unwrap_or_default().parse::<u64>().ok();
                    if let Some(interval) = interval_ms
                        .filter(|ms| *ms > 0)
                        .map(Duration::from_millis)
                        .map(|interval| interval.clamp(MIN_PADDING_INTERVAL, MAX_PADDING_INTERVAL))
                    {
                        disguise.padding_interval = Some(interval);
                    }
                    if let Some(size) = parts.next().and_then(|size| size.parse::<usize>().ok())
                        && size > 0
                    {
                        disguise.padding_size = size.min(MAX_PADDING_SIZE);
                    }
                }
                _ => {}
            }
        }
        disguise
    }

    pub fn user_agent(&self) -> String {
        self.user_agent
            .clone()
            .unwrap_or_else(|| DEFAULT_BROWSER_UA.to_owned())
    }

    pub fn accept_language(&self) -> String {
        self.accept_language
            .clone()
            .unwrap_or_else(|| DEFAULT_ACCEPT_LANGUAGE.to_owned())
    }

    /// SNI presented to the TLS layer. Defaults to the URL host, which keeps
    /// the official behavior when no `sni` param is configured.
    pub fn sni(&self, url: &url::Url) -> String {
        self.sni
            .clone()
            .unwrap_or_else(|| url.domain().unwrap_or("localhost").to_owned())
    }

    /// Builds the HTTP request target (scheme/host/path/query) for the
    /// WebSocket upgrade while stripping the disguise parameters themselves.
    pub fn request_uri(&self, url: &url::Url) -> anyhow::Result<String> {
        let host = self.host.clone().unwrap_or_else(|| self.sni(url));
        let path = self.path.clone().unwrap_or_else(|| {
            if url.path().is_empty() {
                "/".to_owned()
            } else {
                url.path().to_owned()
            }
        });
        let query: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| !DISGUISE_QUERY_KEYS.contains(&key.as_ref()))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let mut request_url = url.clone();
        request_url
            .set_host(Some(host.as_str()))
            .map_err(|_| anyhow::anyhow!("invalid disguise host {host:?}"))?;
        request_url.set_path(&path);
        request_url.query_pairs_mut().clear();
        if !query.is_empty() {
            request_url.query_pairs_mut().extend_pairs(query);
        }
        Ok(request_url.to_string())
    }

    pub fn padding_enabled(&self) -> bool {
        self.padding_interval.is_some()
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

struct WebSocketPacketSink<S> {
    inner: S,
    padding: Option<WebSocketPadding>,
    /// Frames already accepted via `start_send` (real packets and any due
    /// padding) but not yet handed to `inner`. `poll_ready` / `poll_flush` /
    /// `poll_close` only drain this buffer; they never produce new frames, so
    /// the `Sink` contract (poll_ready is side-effect-free beyond readiness)
    /// is honored. Padding is generated in `start_send` instead.
    pending: VecDeque<Message>,
}

struct WebSocketPadding {
    interval: Duration,
    size: usize,
    next_at: std::time::Instant,
}

impl<S> WebSocketPacketSink<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            padding: None,
            pending: VecDeque::new(),
        }
    }

    fn with_padding(inner: S, interval: Duration, size: usize) -> Self {
        Self {
            inner,
            padding: Some(WebSocketPadding {
                interval,
                size,
                next_at: std::time::Instant::now(),
            }),
            pending: VecDeque::new(),
        }
    }

    fn pending_padding(&self) -> bool {
        self.padding
            .as_ref()
            .is_some_and(|padding| std::time::Instant::now() >= padding.next_at)
    }

    fn take_padding_frame(&mut self) -> Message {
        let padding = self.padding.as_mut().expect("padding checked before");
        let mut payload = vec![0u8; padding.size.max(2)];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut payload);
        padding.next_at += padding.interval;
        // Base64 keeps the random payload valid UTF-8 so it can be sent as a
        // WebSocket text frame; text frames are out-of-band from the binary
        // wire format of real ZCPackets, so padding can never collide with
        // legitimate traffic.
        use base64::{Engine as _, prelude::BASE64_STANDARD};
        Message::text(BASE64_STANDARD.encode(payload))
    }

    /// Drain previously-accepted frames from `pending` into `inner`. This is
    /// the only side effect `poll_ready` / `poll_flush` / `poll_close` perform:
    /// pushing already-accepted items downstream, exactly like
    /// `futures::sink::Buffer`. No new frames are produced here.
    fn drain_pending<E>(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TunnelError>>
    where
        S: Sink<Message, Error = E> + Unpin,
        E: std::fmt::Display,
    {
        loop {
            if self.pending.is_empty() {
                return Poll::Ready(Ok(()));
            }
            ready!(
                Pin::new(&mut self.inner)
                    .poll_ready(cx)
                    .map_err(websocket_error)
            )?;
            let frame = self.pending.pop_front().expect("pending was non-empty");
            Pin::new(&mut self.inner)
                .start_send(frame)
                .map_err(websocket_error)?;
        }
    }
}

impl<S, E> Sink<ZCPacket> for WebSocketPacketSink<S>
where
    S: Sink<Message, Error = E> + Unpin,
    E: std::fmt::Display,
{
    type Error = TunnelError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Drain previously-accepted frames first. No new frames are generated
        // here, so `poll_ready` is free of the side effect that previously
        // violated the `Sink` contract.
        ready!(self.drain_pending(cx))?;
        Pin::new(&mut self.inner)
            .poll_ready(cx)
            .map(|result| result.map_err(websocket_error))
    }

    fn start_send(mut self: Pin<&mut Self>, packet: ZCPacket) -> Result<(), Self::Error> {
        // Keep the official zero-copy fast path when padding is disabled. The
        // queue is only needed when an optional disguise frame must precede
        // the packet and the inner sink may accept one frame per readiness
        // notification.
        if self.padding.is_none() && self.pending.is_empty() {
            return Pin::new(&mut self.inner)
                .start_send(Message::binary(packet.tunnel_payload_bytes().freeze()))
                .map_err(websocket_error);
        }
        // Padding is generated here, in `start_send` - a method explicitly
        // allowed to begin sending. The padding frame is enqueued ahead of the
        // real packet so the next `poll_ready` drains them in wire order
        // (padding, then packet), matching the pre-bug behavior. Both items go
        // through `pending` so we never rely on `inner` being ready for two
        // `start_send` calls (the trait only guarantees one slot per
        // `poll_ready`).
        if self.pending_padding() {
            let frame = self.take_padding_frame();
            self.pending.push_back(frame);
        }
        self.pending
            .push_back(Message::binary(packet.tunnel_payload_bytes().freeze()));
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.drain_pending(cx))?;
        Pin::new(&mut self.inner)
            .poll_flush(cx)
            .map(|result| result.map_err(websocket_error))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.drain_pending(cx))?;
        Pin::new(&mut self.inner)
            .poll_close(cx)
            .map(|result| result.map_err(websocket_error))
    }
}

async fn map_from_ws_message(
    message: Result<Message, tokio_websockets::Error>,
    padding_expected: bool,
) -> Option<Result<ZCPacket, TunnelError>> {
    let message = match message {
        Ok(message) => message,
        Err(error) => {
            tracing::error!(?error, "recv from websocket error");
            return Some(Err(websocket_error(error)));
        }
    };
    if message.is_close() {
        tracing::warn!("recv close message from websocket");
        return None;
    }
    if message.is_text() {
        if padding_expected {
            tracing::trace!("dropping websocket text padding frame");
        } else {
            // Still dropped to keep the data path safe, but surfaced loudly:
            // an unexpected text frame usually means a protocol or version
            // mismatch on this connection, which must not vanish silently.
            tracing::warn!(
                "dropping unexpected websocket text frame (padding not enabled for this connection)"
            );
        }
        return None;
    }
    if !message.is_binary() {
        let message = format!("{message:?}");
        tracing::error!(?message, "Invalid packet");
        return Some(Err(TunnelError::InvalidPacket(message)));
    }
    let payload = message.into_payload();
    Some(Ok(ZCPacket::new_from_buf(
        BytesMut::from(payload.as_bytes()),
        ZCPacketType::DummyTunnel,
    )))
}

pub(crate) async fn upgrade_accepted<S>(
    stream: S,
    local_url: url::Url,
) -> Result<Box<dyn Tunnel>, TunnelError>
where
    S: VirtualTcpSocket,
{
    let peer_addr = stream.peer_addr()?;
    let mut remote_url = socket_url(local_url.scheme(), peer_addr);
    let stream = if is_wss(&local_url)? {
        init_crypto_provider();
        let (certificates, private_key) = get_insecure_tls_cert();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)
            .with_context(|| "Failed to create server config")?;
        Either::Left(TlsAcceptor::from(Arc::new(config)).accept(stream).await?)
    } else {
        Either::Right(stream)
    };

    let (request, stream) = ServerBuilder::new()
        .limits(Limits::unlimited())
        .max_headers(128)
        .accept(stream)
        .await
        .map_err(websocket_error)?;

    if trusted_proxy_contains(peer_addr.ip())
        && let Some(forwarded) = request
            .headers()
            .get("Forwarded")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| ForwardedHeaderValue::from_forwarded(value).ok())
            .or_else(|| {
                request
                    .headers()
                    .get("X-Forwarded-For")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| ForwardedHeaderValue::from_x_forwarded_for(value).ok())
            })
        && let Some(ip) = forwarded.remotest_forwarded_for_ip()
    {
        remote_url
            .set_host(Some(&ip.to_string()))
            .map_err(|_| TunnelError::InvalidAddr(format!("invalid forwarded ip {ip}")))?;
        remote_url
            .query_pairs_mut()
            .append_pair("proxy", &peer_addr.to_string());
    }

    let (write, read) = stream.split();
    let remote_url: crate::proto::common::Url = remote_url.into();
    // A padding client dials the URL this listener advertised, so the listener
    // URL itself declares whether text padding frames may arrive.
    let padding_expected = WssDisguise::from_url(&local_url).padding_enabled();
    let info = TunnelInfo {
        tunnel_type: local_url.scheme().to_owned(),
        local_addr: Some(local_url.into()),
        remote_addr: Some(remote_url.clone()),
        resolved_remote_addr: Some(remote_url),
    };
    Ok(Box::new(TunnelWrapper::new(
        read.filter_map(move |message| map_from_ws_message(message, padding_expected)),
        WebSocketPacketSink::new(write),
        Some(info),
    )))
}

fn socket_url(scheme: &str, addr: SocketAddr) -> url::Url {
    let mut url = url::Url::parse(&format!("{scheme}://0.0.0.0"))
        .expect("WebSocket transport scheme should be a valid URL scheme");
    url.set_ip_host(addr.ip()).unwrap();
    url.set_port(Some(addr.port())).unwrap();
    url
}

#[derive(Debug)]
pub struct WsTunnelListener {
    addr: url::Url,
    listener: Option<TcpListener>,
    socket_mark: Option<u32>,
}

impl WsTunnelListener {
    pub fn new(addr: url::Url) -> Self {
        WsTunnelListener {
            addr,
            listener: None,
            socket_mark: None,
        }
    }

    pub fn set_socket_mark(&mut self, socket_mark: Option<u32>) {
        self.socket_mark = socket_mark;
    }

    async fn listen_tunnel(&mut self) -> Result<(), TunnelError> {
        self.listener = None;

        let addr = SocketAddr::from_url(self.addr.clone(), IpVersion::Both).await?;
        let listener = bind::<TcpListener>()
            .addr(addr)
            .only_v6(true)
            .maybe_socket_mark(self.socket_mark)
            .call()
            .await?;

        self.addr
            .set_port(Some(listener.local_addr()?.port()))
            .unwrap();
        self.listener = Some(listener);

        Ok(())
    }

    async fn accept_tunnel(&mut self) -> Result<Box<dyn Tunnel>, TunnelError> {
        loop {
            let listener = self.listener.as_ref().unwrap();
            // only fail on tcp accept error
            let (stream, _) = listener.accept().await?;
            stream.set_nodelay(true).unwrap();
            match timeout(
                SERVER_HANDSHAKE_TIMEOUT,
                upgrade_accepted(RuntimeTcpSocket::new(stream), self.addr.clone()),
            )
            .await
            {
                Ok(Ok(tunnel)) => return Ok(tunnel),
                e => {
                    tracing::error!(?e, ?self, "Failed to accept ws/wss tunnel");
                    continue;
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl easytier_core::socket::SocketListener for WsTunnelListener {
    type Accepted = Box<dyn Tunnel>;

    async fn listen(&mut self) -> anyhow::Result<()> {
        Ok(self.listen_tunnel().await?)
    }

    async fn accept(&mut self) -> anyhow::Result<Self::Accepted> {
        Ok(self.accept_tunnel().await?)
    }

    fn local_url(&self) -> url::Url {
        self.addr.clone()
    }
}

pub(crate) async fn upgrade_connected<S>(
    stream: S,
    remote_url: url::Url,
) -> Result<Box<dyn Tunnel>, TunnelError>
where
    S: VirtualTcpSocket,
{
    let is_wss = is_wss(&remote_url)?;
    let local_addr = stream.local_addr()?;
    let resolved_remote_addr = stream.peer_addr()?;
    let info = TunnelInfo {
        tunnel_type: remote_url.scheme().to_owned(),
        local_addr: Some(
            super::build_url_from_socket_addr(&local_addr.to_string(), remote_url.scheme()).into(),
        ),
        remote_addr: Some(remote_url.clone().into()),
        resolved_remote_addr: Some(
            super::build_url_from_socket_addr(
                &resolved_remote_addr.to_string(),
                remote_url.scheme(),
            )
            .into(),
        ),
    };

    let disguise = WssDisguise::from_url(&remote_url);
    let mut client = if disguise.enabled() {
        let request_uri = disguise
            .request_uri(&remote_url)
            .map_err(|error| TunnelError::InvalidProtocol(error.to_string()))?;
        ClientBuilder::new()
            .uri(&request_uri)
            .map_err(websocket_error)?
    } else {
        // Preserve the official handshake exactly when no disguise query
        // parameter is present.
        ClientBuilder::from_uri(http::Uri::try_from(remote_url.to_string()).unwrap())
    }
    .max_headers(128);
    if disguise.enabled() {
        client = client
            .add_header(
                http::header::USER_AGENT,
                disguise.user_agent().parse().map_err(|_| {
                    TunnelError::InvalidProtocol("invalid User-Agent header".to_owned())
                })?,
            )
            .map_err(websocket_error)?;
        client = client
            .add_header(
                http::header::ACCEPT_LANGUAGE,
                disguise.accept_language().parse().map_err(|_| {
                    TunnelError::InvalidProtocol("invalid Accept-Language header".to_owned())
                })?,
            )
            .map_err(websocket_error)?;
        client = client
            .add_header(
                http::header::ACCEPT,
                "*/*".parse().map_err(|_| {
                    TunnelError::InvalidProtocol("invalid Accept header".to_owned())
                })?,
            )
            .map_err(websocket_error)?;
    }

    let stream: MaybeTlsStream<S> = if is_wss {
        init_crypto_provider();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(get_insecure_tls_client_config()));
        let sni = disguise.sni(&remote_url);
        let server_name = rustls::pki_types::ServerName::try_from(sni)
            .map_err(|_| TunnelError::InvalidProtocol("Invalid SNI".to_owned()))?;
        MaybeTlsStream::Rustls(tls.connect(server_name, stream).await?)
    } else {
        MaybeTlsStream::Plain(stream)
    };

    let (client, _) = client.connect_on(stream).await.map_err(websocket_error)?;
    let (write, read) = client.split();
    let padding_expected = disguise.padding_enabled();
    let write = if let Some(interval) = disguise.padding_interval {
        WebSocketPacketSink::with_padding(write, interval, disguise.padding_size)
    } else {
        WebSocketPacketSink::new(write)
    };
    Ok(Box::new(TunnelWrapper::new(
        read.filter_map(move |message| map_from_ws_message(message, padding_expected)),
        write,
        Some(info),
    )))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use easytier_core::socket::SocketListener;
    use futures::SinkExt;
    use std::io;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpSocket,
    };

    struct FailingWebSocketSink {
        close_called: bool,
    }

    #[derive(Default)]
    struct RecordingWebSocketSink {
        messages: Vec<Message>,
    }

    impl Sink<Message> for RecordingWebSocketSink {
        type Error = io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            self.messages.push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Sink<Message> for FailingWebSocketSink {
        type Error = io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "send failed",
            )))
        }

        fn poll_close(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.close_called = true;
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "close failed",
            )))
        }
    }

    #[tokio::test]
    async fn packet_sink_maps_send_and_close_errors_independently() {
        let mut sink = WebSocketPacketSink::new(FailingWebSocketSink {
            close_called: false,
        });

        sink.send(ZCPacket::new_with_payload(b"packet"))
            .await
            .expect_err("send should fail");
        sink.close().await.expect_err("close should fail");
        assert!(sink.inner.close_called);
    }

    #[tokio::test]
    async fn packet_sink_without_padding_uses_direct_send_path() {
        let mut sink = WebSocketPacketSink::new(RecordingWebSocketSink::default());
        sink.send(ZCPacket::new_with_payload(b"direct packet"))
            .await
            .unwrap();

        assert!(sink.pending.is_empty());
        assert_eq!(sink.inner.messages.len(), 1);
        assert!(sink.inner.messages[0].is_binary());
    }

    #[tokio::test]
    async fn map_from_ws_message_keeps_real_packet_with_zero_first_byte() {
        // A real ZCPacket over a ws/wss tunnel starts with its
        // `PeerManagerHeader`; the first byte is the low byte of
        // `from_peer_id` (little-endian) and can legitimately be 0x00 (e.g.
        // web-secure / foreign-network packets use from_peer_id = 0). Such a
        // binary frame must not be mistaken for padding.
        let packet_bytes = vec![0x00u8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
        let result = map_from_ws_message(Ok(Message::binary(packet_bytes.clone())), true)
            .await
            .expect("real packet must not be dropped");
        let packet = result.expect("real packet must decode");
        assert_eq!(packet.tunnel_payload_bytes().as_ref(), &packet_bytes[..]);
    }

    #[tokio::test]
    async fn map_from_ws_message_drops_text_padding_frame() {
        // Padding is carried as a WebSocket text frame; it must be silently
        // dropped without touching the (encrypted or not) payload.
        let result = map_from_ws_message(Ok(Message::text("cGFkZGluZw==".to_owned())), true).await;
        assert!(result.is_none(), "text padding frame must be dropped");
    }

    #[tokio::test]
    async fn map_from_ws_message_reports_unexpected_text_frame() {
        // Text frames on a connection that did not enable padding are dropped
        // as well (the data path stays text-free), but they indicate a
        // protocol mismatch and must not be silent.
        let result = map_from_ws_message(Ok(Message::text("cGFkZGluZw==".to_owned())), false).await;
        assert!(result.is_none(), "unexpected text frame is still dropped");
    }

    #[tokio::test]
    async fn ws_forwarded() {
        let mut listener = WsTunnelListener::new("ws://127.0.0.1:25559".parse().unwrap());
        listener.listen().await.unwrap();

        let server_task = tokio::spawn(async move {
            let tunnel = listener.accept().await.unwrap();

            let remote_addr = tunnel
                .info()
                .unwrap()
                .remote_addr
                .unwrap()
                .url
                .parse::<url::Url>()
                .unwrap();

            assert_eq!(remote_addr.host_str().unwrap(), "203.0.113.5");
            let proxy_addr = remote_addr
                .query_pairs()
                .find(|(k, _)| k == "proxy")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            assert_eq!(proxy_addr, "127.0.0.1:25560");

            tunnel
        });

        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:25560".parse().unwrap()).unwrap();
        let mut stream = socket
            .connect("127.0.0.1:25559".parse().unwrap())
            .await
            .unwrap();

        let handshake = "GET / HTTP/1.1\r\n\
                         Host: 127.0.0.1:25559\r\n\
                         Upgrade: websocket\r\n\
                         Connection: Upgrade\r\n\
                         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                         Sec-WebSocket-Version: 13\r\n\
                         X-Forwarded-For: 203.0.113.5, 192.168.1.1\r\n\
                         \r\n";

        stream.write_all(handshake.as_bytes()).await.unwrap();

        let mut buf = [0u8; 1024];
        let bytes_read = stream.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..bytes_read]);

        assert!(response.contains("101 Switching Protocols"));

        let _tunnel = server_task.await.unwrap();
    }

    #[test]
    fn wss_disguise_request_uri_applies_path_host_and_strips_params() {
        let url: url::Url =
            "wss://real.example.com:8443/token?sni=www.cloudflare.com&host=api.cloudflare.com&path=/api/v1/query&ua=Mozilla/5.0&padding=2000,256&keep=1"
                .parse()
                .unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert!(disguise.enabled());
        assert_eq!(disguise.sni(&url), "www.cloudflare.com");
        assert_eq!(disguise.user_agent(), "Mozilla/5.0");
        assert_eq!(
            disguise.padding_interval,
            Some(std::time::Duration::from_millis(2000))
        );
        assert_eq!(disguise.padding_size, 256);

        let request_uri = disguise.request_uri(&url).unwrap();
        let parsed: url::Url = request_uri.parse().unwrap();
        assert_eq!(parsed.scheme(), "wss");
        assert_eq!(parsed.host_str(), Some("api.cloudflare.com"));
        assert_eq!(parsed.path(), "/api/v1/query");
        let params: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(params, vec![("keep".to_string(), "1".to_string())]);
        assert!(!request_uri.contains("sni="));
        assert!(!request_uri.contains("ua="));
        assert!(!request_uri.contains("padding="));
    }

    #[test]
    fn wss_disguise_defaults_preserve_official_behavior() {
        let url: url::Url = "wss://host.example.com:443".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert!(!disguise.enabled());
        assert_eq!(disguise.sni(&url), "host.example.com");
        assert!(!disguise.padding_enabled());
        let request_uri = disguise.request_uri(&url).unwrap();
        let parsed: url::Url = request_uri.parse().unwrap();
        assert_eq!(parsed.path(), "/");
        assert_eq!(parsed.host_str(), Some("host.example.com"));
    }

    #[test]
    fn wss_disguise_padding_params_are_clamped_to_safe_bounds() {
        // Padding URLs can come from peer-broadcast listener info, so a
        // malicious peer could advertise padding=1,4000000000 to make this
        // node allocate huge buffers every millisecond. Both values must be
        // clamped to sane bounds.
        let url: url::Url = "wss://h/?padding=1,4000000000".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert_eq!(disguise.padding_interval, Some(MIN_PADDING_INTERVAL));
        assert_eq!(disguise.padding_size, MAX_PADDING_SIZE);

        // An absurdly large interval is harmless but still clamped.
        let url: url::Url = "wss://h/?padding=86400000".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert_eq!(disguise.padding_interval, Some(MAX_PADDING_INTERVAL));
        assert_eq!(disguise.padding_size, DEFAULT_PADDING_SIZE);

        // Values inside the bounds pass through unchanged.
        let url: url::Url = "wss://h/?padding=250,512".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert_eq!(disguise.padding_interval, Some(Duration::from_millis(250)));
        assert_eq!(disguise.padding_size, 512);

        // Zero interval still disables padding entirely.
        let url: url::Url = "wss://h/?padding=0,512".parse().unwrap();
        let disguise = WssDisguise::from_url(&url);
        assert!(!disguise.padding_enabled());
    }

    #[tokio::test]
    async fn wss_disguised_client_handshake_connects_to_plain_server() {
        use easytier_core::socket::SocketListener;
        use futures::{SinkExt, StreamExt};

        let mut listener = WsTunnelListener::new("wss://127.0.0.1:25561".parse().unwrap());
        listener.listen().await.unwrap();

        let server_task = tokio::spawn(async move {
            let tunnel = listener.accept().await.unwrap();
            crate::tunnel::common::tests::_tunnel_echo_server(tunnel, true).await;
        });

        let socket = tokio::net::TcpStream::connect("127.0.0.1:25561")
            .await
            .unwrap();
        socket.set_nodelay(true).unwrap();
        let tunnel = upgrade_connected(
            crate::socket::tcp::RuntimeTcpSocket::new(socket),
            "wss://127.0.0.1:25561/?sni=www.cloudflare.com&path=/api/v1/query&ua=Mozilla/5.0&padding=50,64"
                .parse()
                .unwrap(),
        )
        .await
        .unwrap();
        let (mut read, mut write) = tunnel.split();
        write
            .send(ZCPacket::new_with_payload(b"disguised wss echo"))
            .await
            .unwrap();
        let packet = read.next().await.unwrap().unwrap();
        assert_eq!(packet.payload(), b"disguised wss echo".as_slice());
        let _ = write.close().await;
        server_task.await.unwrap();
    }
}
