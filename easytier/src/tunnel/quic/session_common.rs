//! Session plumbing shared by the `quic://` and `http3://` tunnels: both
//! wrap accepted QUIC connections in the same framed tunnel and drive
//! incoming handshakes through the same semaphore-admitted accept loop. The
//! transports differ only in their TLS/crypto setup, ALPN, negotiated QUIC
//! versions and the scheme advertised in tunnel info, which the callers
//! supply when starting an [`AcceptedSession`].

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use easytier_core::{
    foundation::bandwidth::{CongestionControl, TransmissionWindow, TransmissionWindowSource},
    tunnel::{
        Tunnel, TunnelError,
        framed::{FramedReader, FramedWriter},
        wrapper::TunnelWrapper,
    },
};
use quinn::{
    Connecting, Connection, Endpoint, EndpointConfig, Incoming, ServerConfig, default_runtime,
};
use tokio::{
    sync::{
        OwnedSemaphorePermit, Semaphore,
        mpsc::{Receiver, Sender, channel},
    },
    task::JoinSet,
};
use tokio_util::task::AbortOnDropHandle;

use crate::proto::common::TunnelInfo;

use super::session_socket::QuicUdpSessionSocket;

const QUIC_ACCEPT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum framed tunnel body, including the peer-manager header. Keep both
/// directions of QUIC and HTTP3 at the former client-side receive limit.
pub(crate) const MAX_TUNNEL_BODY_SIZE: usize = 4500;

/// Keeps the endpoint alive for as long as any clone of the connection is
/// in use, and closes the QUIC connection when the last tunnel side drops.
pub(crate) struct ConnWrapper {
    pub(crate) conn: Connection,
    _endpoint: Endpoint,
}

impl ConnWrapper {
    pub(crate) fn new(conn: Connection, endpoint: Endpoint) -> Self {
        Self {
            conn,
            _endpoint: endpoint,
        }
    }
}

impl Drop for ConnWrapper {
    fn drop(&mut self) {
        self.conn.close(0u32.into(), b"done");
    }
}

#[derive(Debug)]
struct QuicWindowSource {
    connection: Connection,
    congestion_control: CongestionControl,
}

impl TransmissionWindowSource for QuicWindowSource {
    fn transmission_window(&self) -> Option<TransmissionWindow> {
        if self.connection.close_reason().is_some() {
            return None;
        }
        let path = self.connection.stats().path;
        let rtt =
            if self.congestion_control == CongestionControl::Bbr && path.min_rtt > Duration::ZERO {
                path.min_rtt
            } else {
                path.rtt
            };
        Some(TransmissionWindow {
            congestion_window_bytes: path.cwnd,
            peer_receive_window_bytes: None,
            rtt,
            congestion_control: self.congestion_control,
        })
    }
}

/// Exposes the live QUIC congestion window as a bandwidth estimate while the
/// connection is open; returns `None` once it has closed.
pub(crate) fn window_source(connection: &Connection) -> Arc<dyn TransmissionWindowSource> {
    // The controller family stays the same for this connection, including
    // migration. Detect it once instead of cloning the controller on each read.
    let congestion_control = if connection
        .congestion_state()
        .into_any()
        .is::<quinn::congestion::Bbr>()
    {
        CongestionControl::Bbr
    } else {
        CongestionControl::LossBased
    };
    Arc::new(QuicWindowSource {
        connection: connection.clone(),
        congestion_control,
    })
}

/// Builds a QUIC endpoint on top of an accepted or connected UDP session
/// socket, resolving the shared async runtime.
pub(crate) fn build_session_endpoint(
    socket: Arc<QuicUdpSessionSocket>,
    server_config: Option<ServerConfig>,
    endpoint_config: EndpointConfig,
) -> Result<Endpoint, TunnelError> {
    let runtime = default_runtime().ok_or(TunnelError::InternalError(
        "no async runtime found".to_owned(),
    ))?;
    Ok(Endpoint::new_with_abstract_socket(
        endpoint_config,
        server_config,
        socket,
        runtime,
    )?)
}

struct PendingSessionTunnel {
    connecting: Connecting,
    endpoint: Endpoint,
    local_url: url::Url,
    remote_addr: SocketAddr,
    _handshake_permit: OwnedSemaphorePermit,
}

async fn finish_session_tunnel(
    pending: PendingSessionTunnel,
    tunnel_type: &str,
) -> Result<Box<dyn Tunnel>, TunnelError> {
    let PendingSessionTunnel {
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
    let connection = Arc::new(ConnWrapper::new(connection, endpoint));
    let bandwidth_source = window_source(&connection.conn);
    let remote_url =
        crate::tunnel::build_url_from_socket_addr(&remote_addr.to_string(), tunnel_type);
    let info = TunnelInfo {
        tunnel_type: tunnel_type.to_owned(),
        local_addr: Some(local_url.into()),
        remote_addr: Some(remote_url.clone().into()),
        resolved_remote_addr: Some(remote_url.into()),
    };
    Ok(Box::new(
        TunnelWrapper::new(
            FramedReader::new_with_associate_data(
                read,
                MAX_TUNNEL_BODY_SIZE,
                Some(Box::new(connection.clone())),
            ),
            FramedWriter::new_with_associate_data(write, Some(Box::new(connection))),
            Some(info),
        )
        .with_bandwidth_source(Some(bandwidth_source)),
    ))
}

async fn run_accepted_session(
    endpoint: Endpoint,
    local_url: url::Url,
    tunnel_type: &'static str,
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
                        format!("{tunnel_type} accept task failed: {error}"),
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
                        complete_tasks.spawn(finish_session_tunnel(
                            PendingSessionTunnel {
                                connecting,
                                endpoint: endpoint.clone(),
                                local_url: local_url.clone(),
                                remote_addr,
                                _handshake_permit: handshake_permit,
                            },
                            tunnel_type,
                        ));
                    }
                    Err(error) => {
                        drop(handshake_permit);
                        if completed
                            .send(Err(anyhow::Error::new(error)
                                .context(format!("{tunnel_type} accept connection failed"))
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

/// Drives incoming QUIC handshakes for one accepted UDP session, admitting
/// at most as many concurrent handshakes as the caller's semaphore allows.
pub(crate) struct AcceptedSession {
    completed: Receiver<Result<Box<dyn Tunnel>, TunnelError>>,
    log_label: &'static str,
    _accept_task: AbortOnDropHandle<()>,
}

impl AcceptedSession {
    /// Starts the accept loop. `tunnel_type` labels the produced tunnels and
    /// error messages; `log_label` keeps the connection-failure warning text
    /// at each transport's established wording (`QUIC` vs `http3`).
    pub(crate) fn start(
        endpoint: Endpoint,
        local_url: url::Url,
        tunnel_type: &'static str,
        log_label: &'static str,
        handshakes: Arc<Semaphore>,
    ) -> Self {
        let (completed_tx, completed) = channel(100);
        let accept_task = AbortOnDropHandle::new(tokio::spawn(run_accepted_session(
            endpoint,
            local_url,
            tunnel_type,
            handshakes,
            completed_tx,
        )));
        Self {
            completed,
            log_label,
            _accept_task: accept_task,
        }
    }

    pub(crate) async fn accept(&mut self) -> Result<Box<dyn Tunnel>, TunnelError> {
        while let Some(result) = self.completed.recv().await {
            match result {
                Ok(tunnel) => return Ok(tunnel),
                Err(error) => {
                    tracing::warn!(?error, "{} session connection failed", self.log_label);
                }
            }
        }
        Err(TunnelError::Shutdown)
    }
}
