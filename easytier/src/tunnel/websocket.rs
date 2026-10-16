use super::FromUrl;
use super::insecure_tls::{get_insecure_tls_cert, get_insecure_tls_client_config};
use crate::proto::common::TunnelInfo;
#[cfg(test)]
use crate::socket::tcp::RuntimeTcpSocket;
use anyhow::Context as _;
use cidr::IpCidr;
use easytier_core::socket::tcp::VirtualTcpListener;
use easytier_core::{
    socket::tcp::VirtualTcpSocket,
    tunnel::{IpVersion, Tunnel, TunnelError, wrapper::TunnelWrapper},
};
use forwarded_header_value::ForwardedHeaderValue;
use futures::StreamExt;
use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, LazyLock},
    time::Duration,
};
#[cfg(test)]
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_util::either::Either;
use tokio_websockets::{ClientBuilder, Limits, MaybeTlsStream, ServerBuilder};

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const SERVER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

mod codec;
mod disguise;
#[cfg(test)]
mod official_interop;

use codec::{
    TRANSPORT_FEATURES_HEADER, WS_TEXT_PADDING_V1, WebSocketPacketSink, map_from_ws_message,
    supports_text_padding,
};
pub use disguise::WssDisguise;

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

pub(crate) async fn upgrade_accepted<S>(
    stream: S,
    local_url: url::Url,
) -> Result<Box<dyn Tunnel>, TunnelError>
where
    S: VirtualTcpSocket,
{
    let peer_addr = stream.peer_addr()?;
    let bandwidth_source = stream.bandwidth_source();
    let mut remote_url = socket_url(local_url.scheme(), peer_addr);
    let stream = if is_wss(&local_url)? {
        let (certificates, private_key) = get_insecure_tls_cert();
        // Do not rely on another tunnel initializing the global provider.
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .with_context(|| "Failed to select TLS protocol versions")?
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
        // This announces receiver support, not an unconditional permission
        // to send padding. Both the offer and this response must contain the
        // feature before the client constructs a padding sink. Negotiating at
        // HTTP upgrade also covers config-server tunnels, which have no peer
        // handshake. The library sends the response before returning request.
        .add_header(
            http::HeaderName::from_static(TRANSPORT_FEATURES_HEADER),
            http::HeaderValue::from_static(WS_TEXT_PADDING_V1),
        )
        .map_err(websocket_error)?
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
            .set_ip_host(ip)
            .map_err(|_| TunnelError::InvalidAddr(format!("invalid forwarded ip {ip}")))?;
        remote_url
            .query_pairs_mut()
            .append_pair("proxy", &peer_addr.to_string());
    }

    let (write, read) = stream.split();
    let remote_url: crate::proto::common::Url = remote_url.into();
    // Listener URL policy cannot establish remote support. The HTTP offer is
    // the remote receiver's explicit promise for this connection only.
    let padding_expected = supports_text_padding(request.headers());
    // Padding is bidirectional: a listener configured with `?padding=` pads
    // its own outgoing frames too, but only when the peer offered the
    // feature in its upgrade request, mirroring the client-side rule that
    // both sides must declare the feature before padding.
    let listener_disguise = WssDisguise::from_url(&local_url);
    let write = if let Some(interval) = listener_disguise
        .padding_interval
        .filter(|_| padding_expected)
    {
        WebSocketPacketSink::with_padding(write, interval, listener_disguise.padding_size)
    } else {
        WebSocketPacketSink::new(write)
    };
    let info = TunnelInfo {
        tunnel_type: local_url.scheme().to_owned(),
        local_addr: Some(local_url.into()),
        remote_addr: Some(remote_url.clone()),
        resolved_remote_addr: Some(remote_url),
    };
    Ok(Box::new(
        TunnelWrapper::new(
            read.filter_map(move |message| map_from_ws_message(message, padding_expected)),
            write,
            Some(info),
        )
        .with_bandwidth_source(bandwidth_source),
    ))
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
    listener: Option<crate::socket::tcp::RuntimeTcpListener>,
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
        let mut options = easytier_core::socket::tcp::TcpListenOptions::direct_connect(addr);
        options.bind.only_v6 = true;
        options.bind.context.socket_mark = self.socket_mark;
        let listener = crate::socket::tcp::bind_tcp_listener(options).await?;

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
            match timeout(
                SERVER_HANDSHAKE_TIMEOUT,
                upgrade_accepted(stream, self.addr.clone()),
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
    let bandwidth_source = stream.bandwidth_source();
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
    let padding_offered = disguise.padding_enabled();
    if padding_offered {
        client = client
            .add_header(
                http::HeaderName::from_static(TRANSPORT_FEATURES_HEADER),
                http::HeaderValue::from_static(WS_TEXT_PADDING_V1),
            )
            .map_err(websocket_error)?;
    }

    let stream: MaybeTlsStream<S> = if is_wss {
        let tls = tokio_rustls::TlsConnector::from(Arc::new(get_insecure_tls_client_config()));
        let sni = disguise.sni(&remote_url);
        let server_name = rustls::pki_types::ServerName::try_from(sni)
            .map_err(|_| TunnelError::InvalidProtocol("Invalid SNI".to_owned()))?;
        MaybeTlsStream::Rustls(tls.connect(server_name, stream).await?)
    } else {
        MaybeTlsStream::Plain(stream)
    };

    let (client, response) = client.connect_on(stream).await.map_err(websocket_error)?;
    let (write, read) = client.split();
    let padding_expected = padding_offered && supports_text_padding(response.headers());
    let write = if let Some(interval) = disguise.padding_interval.filter(|_| padding_expected) {
        WebSocketPacketSink::with_padding(write, interval, disguise.padding_size)
    } else {
        WebSocketPacketSink::new(write)
    };
    Ok(Box::new(
        TunnelWrapper::new(
            read.filter_map(move |message| map_from_ws_message(message, padding_expected)),
            write,
            Some(info),
        )
        .with_bandwidth_source(bandwidth_source),
    ))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use easytier_core::{packet::ZCPacket, socket::SocketListener};
    use futures::SinkExt;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpSocket,
    };
    use tokio_websockets::Message;

    #[tokio::test]
    async fn host_authority_reaches_real_upgrade_without_changing_dial_or_sni() {
        timeout(Duration::from_secs(15), async {
            for scheme in ["ws", "wss"] {
                for host in [
                    "front.example",
                    "front.example:443",
                    "192.0.2.2:9443",
                    "[2001:db8::2]:443",
                ] {
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let address = listener.local_addr().unwrap();
                    let expected_host = if host == "front.example" {
                        format!("front.example:{}", address.port())
                    } else {
                        host.to_owned()
                    };
                    let server = tokio::spawn(async move {
                        let (stream, _) = listener.accept().await.unwrap();
                        let stream = if scheme == "wss" {
                            let (certificates, private_key) = get_insecure_tls_cert();
                            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                                rustls::crypto::ring::default_provider(),
                            ))
                            .with_safe_default_protocol_versions()
                            .unwrap()
                            .with_no_client_auth()
                            .with_single_cert(certificates, private_key)
                            .unwrap();
                            let tls = TlsAcceptor::from(Arc::new(config))
                                .accept(stream)
                                .await
                                .unwrap();
                            assert_eq!(tls.get_ref().1.server_name(), Some("tls.example"));
                            Either::Left(tls)
                        } else {
                            Either::Right(stream)
                        };
                        let (request, mut websocket) =
                            ServerBuilder::new().accept(stream).await.unwrap();
                        assert_eq!(request.headers()[http::header::HOST], expected_host);
                        assert_eq!(
                            request.uri().path_and_query().unwrap().as_str(),
                            "/upgrade?keep=1"
                        );
                        let packet = websocket.next().await.unwrap().unwrap();
                        assert!(packet.is_binary());
                        websocket.send(packet).await.unwrap();
                    });
                    let mut url: url::Url = format!("{scheme}://{address}/original?keep=1")
                        .parse()
                        .unwrap();
                    url.query_pairs_mut()
                        .append_pair("host", host)
                        .append_pair("sni", "tls.example")
                        .append_pair("path", "/upgrade");
                    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
                    let tunnel = upgrade_connected(RuntimeTcpSocket::new(stream), url.clone())
                        .await
                        .unwrap();
                    assert_eq!(
                        tunnel.info().unwrap().remote_addr.unwrap().url,
                        url.as_str()
                    );
                    let (mut read, mut write) = tunnel.split();
                    write
                        .send(ZCPacket::new_with_payload(b"host authority probe"))
                        .await
                        .unwrap();
                    let packet = read.next().await.unwrap().unwrap();
                    assert_eq!(packet.payload(), b"host authority probe");
                    server.await.unwrap();
                }
            }
        })
        .await
        .unwrap();
    }

    async fn padding_upgrade_roundtrip(scheme: &str, response_feature: Option<&'static str>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let tls = scheme == "wss";
        let negotiated = response_feature.is_some_and(|value| {
            value
                .split(',')
                .any(|feature| feature.trim() == WS_TEXT_PADDING_V1)
        });
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = if tls {
                let (certificates, private_key) = get_insecure_tls_cert();
                let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(certificates, private_key)
                .unwrap();
                Either::Left(
                    TlsAcceptor::from(Arc::new(config))
                        .accept(stream)
                        .await
                        .unwrap(),
                )
            } else {
                Either::Right(stream)
            };
            // An unmodified server never advertises padding and rejects text
            // messages. This exercises the first management/RPC packet without
            // a Peer HandshakeRequest or Noise message anywhere in the test.
            let mut builder = ServerBuilder::new();
            if let Some(feature) = response_feature {
                builder = builder
                    .add_header(
                        http::HeaderName::from_static(TRANSPORT_FEATURES_HEADER),
                        feature.parse().unwrap(),
                    )
                    .unwrap();
            }
            let (request, mut websocket) = builder.accept(stream).await.unwrap();
            assert!(supports_text_padding(request.headers()));
            let first = websocket.next().await.unwrap().unwrap();
            let packet = if negotiated {
                assert!(
                    first.is_text(),
                    "acknowledged padding precedes the first packet"
                );
                websocket.next().await.unwrap().unwrap()
            } else {
                first
            };
            assert!(
                packet.is_binary(),
                "official peers must only receive binary packets"
            );
            websocket.send(packet).await.unwrap();
        });
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let tunnel = upgrade_connected(
            RuntimeTcpSocket::new(stream),
            format!("{scheme}://{address}/config-token?padding=100,64")
                .parse()
                .unwrap(),
        )
        .await
        .unwrap();
        let (mut read, mut write) = tunnel.split();
        write
            .send(ZCPacket::new_with_payload(b"first management packet"))
            .await
            .unwrap();
        let packet = timeout(Duration::from_secs(3), read.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(packet.payload(), b"first management packet");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn padding_upgrade_works_for_ws_and_wss_management_tunnels() {
        for scheme in ["ws", "wss"] {
            for response in [
                None,
                Some("ws-text-padding-v2"),
                Some("prefix-ws-text-padding-v1"),
                Some(WS_TEXT_PADDING_V1),
                Some("other-v2, ws-text-padding-v1"),
            ] {
                padding_upgrade_roundtrip(scheme, response).await;
            }
        }
    }

    #[tokio::test]
    async fn listener_accepts_negotiated_padding_without_a_padding_url() {
        let mut listener = WsTunnelListener::new("ws://127.0.0.1:0".parse().unwrap());
        listener.listen().await.unwrap();
        let url = listener.local_url();
        let server = tokio::spawn(async move {
            let tunnel = listener.accept().await.unwrap();
            let (mut read, _) = tunnel.split();
            let packet = read.next().await.unwrap().unwrap();
            assert_eq!(packet.payload(), b"management handshake");
        });
        let (mut client, response) = ClientBuilder::new()
            .uri(url.as_str())
            .unwrap()
            .add_header(
                http::HeaderName::from_static(TRANSPORT_FEATURES_HEADER),
                http::HeaderValue::from_static(WS_TEXT_PADDING_V1),
            )
            .unwrap()
            .connect()
            .await
            .unwrap();
        assert!(supports_text_padding(response.headers()));
        client.send(Message::text("cGFkZGluZw==")).await.unwrap();
        client
            .send(Message::binary(
                ZCPacket::new_with_payload(b"management handshake")
                    .tunnel_payload_bytes()
                    .freeze(),
            ))
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn listener_padding_url_does_not_negotiate_for_an_official_client() {
        let mut listener =
            WsTunnelListener::new("ws://127.0.0.1:0/?padding=100,64".parse().unwrap());
        listener.listen().await.unwrap();
        let url = listener.local_url();
        let server = tokio::spawn(async move {
            let tunnel = listener.accept().await.unwrap();
            let (mut read, _) = tunnel.split();
            assert!(matches!(
                read.next().await,
                Some(Err(TunnelError::InvalidPacket(_)))
            ));
        });
        let (mut client, _) = ClientBuilder::new()
            .uri(url.as_str())
            .unwrap()
            .connect()
            .await
            .unwrap();
        client.send(Message::text("cGFkZGluZw==")).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn listener_padding_url_pads_server_frames_for_negotiating_client() {
        let mut listener =
            WsTunnelListener::new("ws://127.0.0.1:0/?padding=100,64".parse().unwrap());
        listener.listen().await.unwrap();
        let url = listener.local_url();
        let server = tokio::spawn(async move {
            let tunnel = listener.accept().await.unwrap();
            let (mut read, mut write) = tunnel.split();
            let packet = read.next().await.unwrap().unwrap();
            assert_eq!(packet.payload(), b"negotiation probe");
            write
                .send(ZCPacket::new_with_payload(b"server reply"))
                .await
                .unwrap();
        });
        let (mut client, response) = ClientBuilder::new()
            .uri(url.as_str())
            .unwrap()
            .add_header(
                http::HeaderName::from_static(TRANSPORT_FEATURES_HEADER),
                http::HeaderValue::from_static(WS_TEXT_PADDING_V1),
            )
            .unwrap()
            .connect()
            .await
            .unwrap();
        assert!(supports_text_padding(response.headers()));
        client
            .send(Message::binary(
                ZCPacket::new_with_payload(b"negotiation probe")
                    .tunnel_payload_bytes()
                    .freeze(),
            ))
            .await
            .unwrap();
        // The listener URL configured padding and the client offered the
        // feature, so the server's first outgoing send must be preceded by a
        // text padding frame, mirroring the client-side behavior.
        let first = timeout(Duration::from_secs(3), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(first.is_text(), "server padding frame must precede replies");
        let second = timeout(Duration::from_secs(3), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(second.is_binary());
        server.await.unwrap();
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
