//! Additions to the `quic://` tunnel that the `http3://` tunnel shares: the
//! frame size limit, the bandwidth estimate read from a QUIC connection, and
//! endpoint construction on a UDP session socket.

use std::{sync::Arc, time::Duration};

use easytier_core::{
    foundation::bandwidth::{CongestionControl, TransmissionWindow, TransmissionWindowSource},
    tunnel::TunnelError,
};
use quinn::{Connection, Endpoint, EndpointConfig, ServerConfig, default_runtime};

use super::{ConnWrapper, session_socket::QuicUdpSessionSocket};

/// Maximum framed tunnel body, including the peer-manager header. Keep both
/// directions of QUIC and HTTP3 at the former client-side receive limit.
pub(crate) const MAX_TUNNEL_BODY_SIZE: usize = 4500;

impl ConnWrapper {
    pub(crate) fn new(conn: Connection, endpoint: Endpoint) -> Self {
        Self {
            conn,
            _endpoint: endpoint,
        }
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
