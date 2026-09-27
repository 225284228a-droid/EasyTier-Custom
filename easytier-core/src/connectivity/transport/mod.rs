use std::future::Future;

use futures::{StreamExt, stream::FuturesUnordered};
use url::Url;

use crate::connectivity::protocol::{ProtocolTransport, protocol_transport};
use crate::socket::tcp::TcpSocketPurpose;

mod tcp;
mod udp;

pub(crate) use tcp::connect_tcp;
pub(crate) use udp::connect_udp_with;
pub use udp::{ConnectedUdpSession, UdpSessionMode, connect_udp};

/// The transport a dialer uses for a URL scheme: TCP (tagged with the
/// dialer's socket purpose; FakeTcp always keeps its dedicated purpose),
/// UDP (EasyTier mux or a classified protocol), or a host-created byte
/// stream for ring/unix endpoints, which have no IP address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IpTransport {
    Tcp(TcpSocketPurpose),
    Udp(UdpSessionMode),
    ByteStream,
}

impl IpTransport {
    /// Classifies a URL's scheme into its transport. Returns `None` for
    /// schemes no core dialer can handle; callers decide how to report them.
    pub(crate) fn from_url(url: &Url, connect_purpose: TcpSocketPurpose) -> Option<Self> {
        match protocol_transport(url.scheme()) {
            Some(ProtocolTransport::Tcp) => Some(Self::Tcp(connect_purpose)),
            Some(ProtocolTransport::FakeTcp) => Some(Self::Tcp(TcpSocketPurpose::FakeTcp)),
            Some(ProtocolTransport::Udp(mode)) => Some(Self::Udp(mode)),
            None if matches!(url.scheme(), "ring" | "unix") => Some(Self::ByteStream),
            None => None,
        }
    }

    pub(crate) fn is_udp(self) -> bool {
        matches!(self, Self::Udp(_))
    }

    /// FakeTCP dials raw SYN packets and byte streams are host-created, so
    /// neither can bind to a specific interface first.
    pub(crate) fn supports_interface_bind(self) -> bool {
        !matches!(
            self,
            Self::Tcp(TcpSocketPurpose::FakeTcp) | Self::ByteStream
        )
    }
}

/// A host-created non-IP byte stream with host-provided endpoint metadata.
///
/// Unix and in-process transports use the same stream framing boundary as TCP,
/// but their endpoints cannot be represented by `SocketAddr`.
pub struct ConnectedByteStream<S> {
    socket: S,
    local_url: Option<Url>,
    remote_url: Url,
    resolved_remote_url: Option<Url>,
}

impl<S> ConnectedByteStream<S> {
    pub fn new(
        socket: S,
        local_url: Option<Url>,
        remote_url: Url,
        resolved_remote_url: Option<Url>,
    ) -> Self {
        Self {
            socket,
            local_url,
            remote_url,
            resolved_remote_url,
        }
    }

    pub fn into_parts(self) -> (S, Option<Url>, Url, Option<Url>) {
        (
            self.socket,
            self.local_url,
            self.remote_url,
            self.resolved_remote_url,
        )
    }
}

/// A transport endpoint established by a connectivity strategy.
///
/// Manual, direct, and hole-punch strategies stop at this boundary. Protocol
/// code consumes the endpoint and upgrades it into an EasyTier tunnel.
pub enum ConnectedTransport<TcpSocket> {
    Tcp(TcpSocket),
    Udp(ConnectedUdpSession),
    ByteStream(ConnectedByteStream<TcpSocket>),
}

async fn first_success<F, T>(mut futures: FuturesUnordered<F>) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>> + Send,
{
    let mut last_error = None;
    while let Some(result) = futures.next().await {
        match result {
            Ok(value) => return Ok(value),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no transport candidates")))
}
