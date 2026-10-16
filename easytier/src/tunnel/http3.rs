//! HTTP/3-disguised framed QUIC tunnel.
//!
//! Unlike the plain `quic://` tunnel (which uses a fake crypto layer and no
//! ALPN), `http3://` speaks real TLS 1.3 with the `h3` ALPN and a configurable
//! SNI. Its UDP datagrams use native QUIC framing, while its bidirectional
//! application stream carries EasyTier frames rather than HTTP/3 requests and
//! control streams. This provides passive protocol/SNI camouflage, not a
//! guarantee of browser fingerprints or HTTP/3 active-probe compatibility.
//!
//! Usage:
//! - listen: `--listen http3://0.0.0.0:11014`
//! - peer: `http3://host:11014?sni=www.example.com` (SNI defaults to the host)

use crate::proto::common::TunnelInfo;
use anyhow::Context;
use easytier_core::{
    connectivity::{
        protocol::{ServerProtocolAdmission, ServerTunnelAcceptor},
        transport::ConnectedUdpSession,
    },
    socket::udp::UdpSession,
    tunnel::{
        Tunnel, TunnelError,
        framed::{FramedReader, FramedWriter},
        wrapper::TunnelWrapper,
    },
};
use quinn::{
    AsyncUdpSocket, ClientConfig, EndpointConfig, ServerConfig, TransportConfig,
    crypto::rustls::QuicClientConfig, crypto::rustls::QuicServerConfig,
};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::insecure_tls::{SkipServerVerification, get_insecure_tls_cert, sni_from_url};
pub(crate) use super::quic::QuicUdpSessionSocket as Http3UdpSessionSocket;
use super::quic::{session_common, window_source};

pub fn transport_config() -> Arc<TransportConfig> {
    super::quic::transport_config()
}

fn build_rustls_client_config() -> anyhow::Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| anyhow::anyhow!("http3 TLS 1.3 setup failed: {error}"))?
        .dangerous()
        .with_custom_certificate_verifier(SkipServerVerification::new(provider))
        .with_no_client_auth();
    // Mimic a browser's HTTP/3 ALPN.
    config.alpn_protocols = vec![b"h3".to_vec()];
    config.enable_early_data = false;
    Ok(config)
}

fn build_rustls_server_config() -> anyhow::Result<rustls::ServerConfig> {
    let (certificates, private_key) = get_insecure_tls_cert();
    let mut config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|error| anyhow::anyhow!("http3 TLS 1.3 setup failed: {error}"))?
    .with_no_client_auth()
    .with_single_cert(certificates, private_key)
    .with_context(|| "Failed to create http3 server config")?;
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

/// Resolves the SNI for the connection: the `sni` query parameter overrides
/// the URL host, which is what an SNI whitelist will see.
fn resolve_sni(url: &url::Url) -> String {
    let param = url
        .query_pairs()
        .find(|(key, _)| key == "sni")
        .map(|(_, value)| value.into_owned());
    sni_from_url(url, param.as_deref())
}

pub(crate) async fn upgrade_connected(
    connected: ConnectedUdpSession,
    remote_url: url::Url,
) -> Result<Box<dyn Tunnel>, TunnelError> {
    let socket = Arc::new(Http3UdpSessionSocket::new(connected)?);
    let local_addr = socket.local_addr()?;
    let remote_addr = socket.peer_addr();
    let mut endpoint = session_common::build_session_endpoint(socket, None, endpoint_config())?;
    let client_config =
        client_config().map_err(|error| TunnelError::InternalError(error.to_string()))?;
    endpoint.set_default_client_config(client_config);
    let sni = resolve_sni(&remote_url);
    let connecting = endpoint
        .connect(remote_addr, &sni)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("failed to start connection to {remote_addr} with sni {sni}"))?;
    let connection = connecting
        .await
        .with_context(|| format!("failed to connect to {remote_addr}"))?;
    let (write, read) = connection
        .open_bi()
        .await
        .with_context(|| "open_bi failed")?;
    let resolved_remote_addr = connection.remote_address();
    let connection = Arc::new(session_common::ConnWrapper::new(connection, endpoint));
    let bandwidth_source = window_source(&connection.conn);
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
    Ok(Box::new(
        TunnelWrapper::new(
            FramedReader::new_with_associate_data(
                read,
                session_common::MAX_TUNNEL_BODY_SIZE,
                Some(Box::new(connection.clone())),
            ),
            FramedWriter::new_with_associate_data(write, Some(Box::new(connection))),
            Some(info),
        )
        .with_bandwidth_source(Some(bandwidth_source)),
    ))
}

pub(crate) struct Http3AcceptedSession {
    inner: session_common::AcceptedSession,
}

impl Http3AcceptedSession {
    pub(crate) fn new(
        session: UdpSession,
        local_url: url::Url,
        admission: ServerProtocolAdmission,
    ) -> Result<Self, TunnelError> {
        let (active_session, handshake_slots) = admission.into_parts();
        Self::new_with_admission_parts(session, local_url, active_session, handshake_slots)
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
        let server_config =
            server_config().map_err(|error| TunnelError::InternalError(error.to_string()))?;
        let endpoint =
            session_common::build_session_endpoint(socket, Some(server_config), endpoint_config())?;
        Ok(Self {
            inner: session_common::AcceptedSession::start(
                endpoint, local_url, "http3", "http3", handshakes,
            ),
        })
    }

    pub(crate) async fn accept(&mut self) -> Result<Box<dyn Tunnel>, TunnelError> {
        self.inner.accept().await
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
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use easytier_core::{
        connectivity::{
            protocol::ServerProtocolAdmissionController,
            transport::{UdpSessionMode, connect_udp},
        },
        packet::ZCPacket,
        socket::SocketListener,
        socket::udp::{UdpBindOptions, UdpSessionLayer, VirtualUdpSocket, VirtualUdpSocketFactory},
    };
    use futures::{SinkExt, StreamExt};
    use quinn::{Endpoint, default_runtime};

    use crate::host_runtime::native_host_runtime;

    use super::super::quic::session_tests::{
        accept_two_connections_on_same_session, assert_bidirectional_frame_boundaries,
        assert_second_connection_survives_first_close, connected_udp_session_fixture,
    };

    use super::*;

    #[test]
    fn http3_tls_uses_h3_alpn_and_explicit_sni() {
        assert_eq!(
            build_rustls_client_config().unwrap().alpn_protocols,
            vec![b"h3".to_vec()]
        );
        assert_eq!(
            build_rustls_server_config().unwrap().alpn_protocols,
            vec![b"h3".to_vec()]
        );

        let disguised: url::Url = "http3://192.0.2.1:11014?sni=www.example.com"
            .parse()
            .unwrap();
        assert_eq!(resolve_sni(&disguised), "www.example.com");

        let default: url::Url = "http3://origin.example.com:11014".parse().unwrap();
        assert_eq!(resolve_sni(&default), "origin.example.com");
    }

    #[rstest::rstest]
    #[case("http3://[::1]:11014", "::1")]
    #[case("http3://[2001:db8::1]:11014?sni=", "2001:db8::1")]
    #[case("http3://[::1]:11014?sni=www.example.com", "www.example.com")]
    #[test]
    fn http3_ipv6_resolves_a_valid_tls_server_name(#[case] url: &str, #[case] expected: &str) {
        let server_name = resolve_sni(&url.parse().unwrap());
        assert_eq!(server_name, expected);
        assert!(rustls::pki_types::ServerName::try_from(server_name).is_ok());
    }

    #[tokio::test]
    async fn http3_endpoints_use_bbr() {
        super::super::quic::tests::assert_endpoint_controllers(
            client_config().unwrap(),
            server_config().unwrap(),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http3_tunnel_enforces_bidirectional_frame_boundary() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut listener, connected, _) = connected_udp_session_fixture("http3").await;
            let local_url = listener.local_url();
            let remote_url = local_url.clone();
            let server_task = tokio::spawn(async move {
                let session = listener.accept().await.unwrap();
                let admission = ServerProtocolAdmissionController::quic()
                    .try_admit()
                    .unwrap();
                let mut accepted =
                    Http3AcceptedSession::new(session, local_url, admission).unwrap();
                accepted.accept().await.unwrap()
            });
            let client = upgrade_connected(connected, remote_url).await.unwrap();
            assert_bidirectional_frame_boundaries(client, server_task).await;
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http3_upgrades_hole_punch_style_udp_mux_session() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let host = native_host_runtime();
            let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
            let server_socket = host
                .bind_udp(UdpBindOptions::port_bound_listener(bind_addr).with_only_v6(false))
                .await
                .unwrap();
            let server_addr = server_socket.local_addr().unwrap();
            let server_layer = Arc::new(UdpSessionLayer::new(server_socket));
            let server_task = tokio::spawn(async move {
                let session = server_layer.accept().await.unwrap();
                let admission = ServerProtocolAdmissionController::quic()
                    .try_admit()
                    .unwrap();
                let local_url = format!("http3://{server_addr}").parse().unwrap();
                let mut accepted =
                    Http3AcceptedSession::new(session, local_url, admission).unwrap();
                accepted.accept().await.unwrap()
            });

            let connected = tokio::time::timeout(
                Duration::from_secs(3),
                connect_udp(
                    host,
                    server_addr,
                    Vec::new(),
                    UdpBindOptions::hole_punch_candidate(),
                    UdpSessionMode::EasyTierMux,
                ),
            )
            .await
            .expect("UDP mux connect timed out")
            .unwrap();
            let remote_url = format!("http3://{server_addr}").parse().unwrap();
            let client = tokio::time::timeout(
                Duration::from_secs(6),
                upgrade_connected(connected, remote_url),
            )
            .await
            .expect("HTTP3 client upgrade timed out")
            .unwrap();
            assert_eq!(client.info().unwrap().tunnel_type, "http3");
            let (_client_reader, mut client_writer) = client.split();
            client_writer
                .send(ZCPacket::new_with_payload(b"http3 over punched udp mux"))
                .await
                .unwrap();
            let server = tokio::time::timeout(Duration::from_secs(3), server_task)
                .await
                .expect("HTTP3 server accept timed out")
                .unwrap();
            assert_eq!(server.info().unwrap().tunnel_type, "http3");
            let (mut server_reader, _server_writer) = server.split();
            let packet = server_reader.next().await.unwrap().unwrap();
            assert_eq!(packet.payload(), b"http3 over punched udp mux");
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn accepted_udp_session_supports_multiple_http3_connections() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut listener, connected, remote_addr) =
                connected_udp_session_fixture("http3").await;
            let local_url = listener.local_url();
            let socket = Arc::new(Http3UdpSessionSocket::new(connected).unwrap());
            let runtime = default_runtime().unwrap();
            let mut endpoint =
                Endpoint::new_with_abstract_socket(endpoint_config(), None, socket, runtime)
                    .unwrap();
            endpoint.set_default_client_config(client_config().unwrap());

            let server_task = tokio::spawn(async move {
                let session = listener.accept().await.unwrap();
                let admission = ServerProtocolAdmissionController::new(1, 2)
                    .try_admit()
                    .unwrap();
                let mut accepted =
                    Http3AcceptedSession::new(session, local_url, admission).unwrap();
                accept_two_connections_on_same_session(&mut accepted, &mut listener).await
            });

            assert_second_connection_survives_first_close(
                endpoint,
                remote_addr,
                server_task,
                "http3",
            )
            .await;
        })
        .await
        .unwrap();
    }
}
