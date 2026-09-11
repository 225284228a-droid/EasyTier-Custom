//! HTTP/3 disguised QUIC tunnel.
//!
//! Unlike the plain `quic://` tunnel (which uses a fake crypto layer and no
//! ALPN), `http3://` speaks real TLS 1.3 with the `h3` ALPN and a configurable
//! SNI, so the wire traffic is indistinguishable from a browser's HTTP/3
//! connection and can pass SNI-whitelist QoS policies.
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
    AsyncUdpSocket, ClientConfig, Connecting, Connection, Endpoint, EndpointConfig, Incoming,
    ServerConfig, TransportConfig, crypto::rustls::QuicClientConfig,
    crypto::rustls::QuicServerConfig, default_runtime,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    sync::{
        OwnedSemaphorePermit, Semaphore,
        mpsc::{Receiver, Sender, channel},
    },
    task::JoinSet,
};
use tokio_util::task::AbortOnDropHandle;

use super::insecure_tls::{SkipServerVerification, get_insecure_tls_cert, init_crypto_provider};
pub(crate) use super::quic::QuicUdpSessionSocket as Http3UdpSessionSocket;

const QUIC_ACCEPT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);

pub fn transport_config(enable_bbr: bool) -> Arc<TransportConfig> {
    super::quic::transport_config(enable_bbr)
}

fn build_rustls_client_config() -> anyhow::Result<rustls::ClientConfig> {
    init_crypto_provider();
    let provider = rustls::crypto::CryptoProvider::get_default().unwrap();
    let mut config = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|error| anyhow::anyhow!("http3 TLS 1.3 setup failed: {error}"))?
    .dangerous()
    .with_custom_certificate_verifier(SkipServerVerification::new(provider.clone()))
    .with_no_client_auth();
    // Mimic a browser's HTTP/3 ALPN.
    config.alpn_protocols = vec![b"h3".to_vec()];
    config.enable_early_data = false;
    Ok(config)
}

fn build_rustls_server_config() -> anyhow::Result<rustls::ServerConfig> {
    init_crypto_provider();
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

pub fn server_config(enable_bbr: bool) -> anyhow::Result<ServerConfig> {
    let mut config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(
        build_rustls_server_config()?,
    )?));
    config.transport_config(transport_config(enable_bbr));
    Ok(config)
}

pub fn client_config(enable_bbr: bool) -> anyhow::Result<ClientConfig> {
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(
        build_rustls_client_config()?,
    )?));
    config.transport_config(transport_config(enable_bbr));
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
    url.query_pairs()
        .find(|(key, _)| key == "sni")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| url.host_str().unwrap_or("localhost").to_owned())
}

struct ConnWrapper {
    conn: Connection,
    _endpoint: Endpoint,
}

impl Drop for ConnWrapper {
    fn drop(&mut self) {
        self.conn.close(0u32.into(), b"done");
    }
}

pub(crate) async fn upgrade_connected(
    connected: ConnectedUdpSession,
    remote_url: url::Url,
    enable_bbr: bool,
) -> Result<Box<dyn Tunnel>, TunnelError> {
    let socket = Arc::new(Http3UdpSessionSocket::new(connected)?);
    let local_addr = socket.local_addr()?;
    let remote_addr = socket.peer_addr();
    let runtime = default_runtime().ok_or(TunnelError::InternalError(
        "no async runtime found".to_owned(),
    ))?;
    let mut endpoint =
        Endpoint::new_with_abstract_socket(endpoint_config(), None, socket, runtime)?;
    let client_config =
        client_config(enable_bbr).map_err(|error| TunnelError::InternalError(error.to_string()))?;
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
    let connection = Arc::new(ConnWrapper {
        conn: connection,
        _endpoint: endpoint,
    });
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
    Ok(Box::new(TunnelWrapper::new(
        FramedReader::new_with_associate_data(read, 4500, Some(Box::new(connection.clone()))),
        FramedWriter::new_with_associate_data(write, Some(Box::new(connection))),
        Some(info),
    )))
}

struct PendingHttp3SessionTunnel {
    connecting: Connecting,
    endpoint: Endpoint,
    local_url: url::Url,
    remote_addr: SocketAddr,
    _handshake_permit: OwnedSemaphorePermit,
}

async fn finish_http3_session_tunnel(
    pending: PendingHttp3SessionTunnel,
) -> Result<Box<dyn Tunnel>, TunnelError> {
    let PendingHttp3SessionTunnel {
        connecting,
        endpoint,
        local_url,
        remote_addr,
        _handshake_permit,
    } = pending;
    let connection = tokio::time::timeout(QUIC_ACCEPT_COMPLETION_TIMEOUT, connecting)
        .await
        .map_err(TunnelError::Timeout)?
        .with_context(|| "accept connection failed")?;
    let (write, read) =
        tokio::time::timeout(QUIC_ACCEPT_COMPLETION_TIMEOUT, connection.accept_bi())
            .await
            .map_err(TunnelError::Timeout)?
            .with_context(|| "accept_bi failed")?;
    let connection = Arc::new(ConnWrapper {
        conn: connection,
        _endpoint: endpoint,
    });
    let remote_url = super::build_url_from_socket_addr(&remote_addr.to_string(), "http3");
    let info = TunnelInfo {
        tunnel_type: "http3".to_owned(),
        local_addr: Some(local_url.into()),
        remote_addr: Some(remote_url.clone().into()),
        resolved_remote_addr: Some(remote_url.into()),
    };
    Ok(Box::new(TunnelWrapper::new(
        FramedReader::new_with_associate_data(read, 2000, Some(Box::new(connection.clone()))),
        FramedWriter::new_with_associate_data(write, Some(Box::new(connection))),
        Some(info),
    )))
}

async fn run_http3_accepted_session(
    endpoint: Endpoint,
    local_url: url::Url,
    handshakes: Arc<Semaphore>,
    completed: Sender<Result<Box<dyn Tunnel>, TunnelError>>,
) {
    let mut complete_tasks = JoinSet::new();
    let mut pending_incoming: Option<Incoming> = None;
    loop {
        tokio::select! {
            Some(result) = complete_tasks.join_next(), if !complete_tasks.is_empty() => {
                let result = match result {
                    Ok(result) => result,
                    Err(error) => Err(TunnelError::InternalError(
                        format!("http3 accept task failed: {error}"),
                    )),
                };
                if completed.send(result).await.is_err() {
                    break;
                }
            }
            incoming = endpoint.accept(), if pending_incoming.is_none() => {
                match incoming {
                    Some(incoming) => pending_incoming = Some(incoming),
                    None => break,
                }
            }
            permit = handshakes.clone().acquire_owned(), if pending_incoming.is_some() => {
                let Ok(handshake_permit) = permit else {
                    break;
                };
                let incoming = pending_incoming.take().unwrap();
                let remote_addr = incoming.remote_address();
                match incoming.accept() {
                    Ok(connecting) => {
                        complete_tasks.spawn(finish_http3_session_tunnel(
                            PendingHttp3SessionTunnel {
                                connecting,
                                endpoint: endpoint.clone(),
                                local_url: local_url.clone(),
                                remote_addr,
                                _handshake_permit: handshake_permit,
                            },
                        ));
                    }
                    Err(error) => {
                        drop(handshake_permit);
                        if completed
                            .send(Err(anyhow::Error::new(error)
                                .context("http3 accept connection failed")
                                .into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    }
}

pub(crate) struct Http3AcceptedSession {
    completed: Receiver<Result<Box<dyn Tunnel>, TunnelError>>,
    _accept_task: AbortOnDropHandle<()>,
}

impl Http3AcceptedSession {
    pub(crate) fn new(
        session: UdpSession,
        local_url: url::Url,
        admission: ServerProtocolAdmission,
        enable_bbr: bool,
    ) -> Result<Self, TunnelError> {
        let (active_session, handshake_slots) = admission.into_parts();
        Self::new_with_admission_parts(session, local_url, active_session, handshake_slots, enable_bbr)
    }

    fn new_with_admission_parts(
        session: UdpSession,
        local_url: url::Url,
        active_session: OwnedSemaphorePermit,
        handshakes: Arc<Semaphore>,
        enable_bbr: bool,
    ) -> Result<Self, TunnelError> {
        let socket = Arc::new(Http3UdpSessionSocket::from_accepted(
            session,
            active_session,
        )?);
        let runtime = default_runtime().ok_or(TunnelError::InternalError(
            "no async runtime found".to_owned(),
        ))?;
        let server_config =
            server_config(enable_bbr).map_err(|error| TunnelError::InternalError(error.to_string()))?;
        let endpoint = Endpoint::new_with_abstract_socket(
            endpoint_config(),
            Some(server_config),
            socket,
            runtime,
        )?;
        let (completed_tx, completed) = channel(100);
        let accept_task = AbortOnDropHandle::new(tokio::spawn(run_http3_accepted_session(
            endpoint,
            local_url,
            handshakes,
            completed_tx,
        )));
        Ok(Self {
            completed,
            _accept_task: accept_task,
        })
    }

    pub(crate) async fn accept(&mut self) -> Result<Box<dyn Tunnel>, TunnelError> {
        while let Some(result) = self.completed.recv().await {
            match result {
                Ok(tunnel) => return Ok(tunnel),
                Err(error) => {
                    tracing::warn!(?error, "http3 session connection failed");
                }
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
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use easytier_core::{
        connectivity::{
            protocol::ServerProtocolAdmissionController,
            transport::{UdpSessionMode, connect_udp},
        },
        packet::ZCPacket,
        socket::SocketListener,
        socket::udp::{
            UdpBindOptions, UdpSessionAcceptKind, UdpSessionListenRequest, UdpSessionProtocol,
            VirtualUdpSocket,
        },
    };
    use futures::{SinkExt, StreamExt};

    use crate::{
        common::netns::NetNS, host_runtime::native_host_runtime,
        socket::udp::new_runtime_udp_session_listener, tunnel::common::tests::_tunnel_echo_server,
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
    #[tokio::test]
    async fn bbr_is_selected_independently_for_each_http3_endpoint(
        #[values(false, true)] client_bbr: bool,
        #[values(false, true)] server_bbr: bool,
    ) {
        super::super::quic::tests::assert_endpoint_controllers(
            client_config(client_bbr).unwrap(),
            server_config(server_bbr).unwrap(),
            client_bbr,
            server_bbr,
        )
        .await;
    }

    #[rstest::rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn accepted_udp_session_supports_multiple_http3_connections(
        #[values(false, true)] enable_bbr: bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
            let mut listener = new_runtime_udp_session_listener(
                format!("http3://{bind_addr}").parse().unwrap(),
                UdpSessionListenRequest::new(
                    UdpBindOptions::port_bound_listener(bind_addr).with_only_v6(false),
                ),
                UdpSessionAcceptKind::Classified(UdpSessionProtocol::Quic),
                NetNS::new(None),
            );
            listener.listen().await.unwrap();
            let remote_addr = listener.bound_socket().unwrap().local_addr().unwrap();
            let local_url = listener.local_url();

            let connected = connect_udp(
                native_host_runtime(),
                remote_addr,
                Vec::new(),
                UdpBindOptions::direct_connect(),
                UdpSessionMode::Classified(UdpSessionProtocol::Quic),
            )
            .await
            .unwrap();
            let socket = Arc::new(Http3UdpSessionSocket::new(connected).unwrap());
            let runtime = default_runtime().unwrap();
            let mut endpoint =
                Endpoint::new_with_abstract_socket(endpoint_config(), None, socket, runtime)
                    .unwrap();
            endpoint.set_default_client_config(client_config(enable_bbr).unwrap());

            let server_task = tokio::spawn(async move {
                let session = listener.accept().await.unwrap();
                let admission = ServerProtocolAdmissionController::new(1, 2)
                    .try_admit()
                    .unwrap();
                let mut accepted =
                    Http3AcceptedSession::new(session, local_url, admission, enable_bbr).unwrap();
                let first = accepted.accept().await.unwrap();
                let second = accepted.accept().await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err(),
                    "both http3 connections must use the first accepted UDP session"
                );
                (first, second)
            });

            let first_connection = endpoint
                .connect(remote_addr, "localhost")
                .unwrap()
                .await
                .unwrap();
            super::super::quic::tests::assert_bbr_controller(&first_connection, enable_bbr);
            let (first_write, first_read) = first_connection.open_bi().await.unwrap();
            let mut first_send = FramedWriter::new(first_write);
            first_send
                .send(ZCPacket::new_with_payload(b"first http3 connection"))
                .await
                .unwrap();
            let second_connection = endpoint
                .connect(remote_addr, "localhost")
                .unwrap()
                .await
                .unwrap();
            let (second_write, second_read) = second_connection.open_bi().await.unwrap();
            let mut second_send = FramedWriter::new(second_write);
            second_send
                .send(ZCPacket::new_with_payload(b"second http3 connection ready"))
                .await
                .unwrap();
            let (first_server, second_server) = server_task.await.unwrap();

            drop(first_send);
            drop(first_read);
            drop(first_server);
            first_connection.close(0u32.into(), b"first connection done");

            let echo_task = tokio::spawn(_tunnel_echo_server(second_server, false));
            let mut recv = FramedReader::new(second_read, 4500);
            let ready = recv.next().await.unwrap().unwrap();
            assert_eq!(ready.payload(), b"second http3 connection ready".as_slice());
            second_send
                .send(ZCPacket::new_with_payload(
                    b"second http3 connection after first closed",
                ))
                .await
                .unwrap();
            let packet = recv.next().await.unwrap().unwrap();
            assert_eq!(
                packet.payload(),
                b"second http3 connection after first closed".as_slice()
            );
            let _ = second_send.close().await;
            echo_task.await.unwrap();
            second_connection.close(0u32.into(), b"second connection done");
            endpoint.close(0u32.into(), b"test done");
        })
        .await
        .unwrap();
    }
}
