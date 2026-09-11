use std::{net::SocketAddr, sync::Arc};

use crate::{
    connectivity::transport::ConnectedUdpSession,
    socket::udp::{UdpSessionLayer, UdpSessionProtocol, VirtualUdpSocketFactory},
};

use super::DirectConnectorHost;

pub(super) async fn connect_with_socket<H>(
    host: Arc<H>,
    socket: Arc<<H as VirtualUdpSocketFactory>::Socket>,
    remote_addr: SocketAddr,
    classify_quic: bool,
) -> anyhow::Result<ConnectedUdpSession>
where
    H: DirectConnectorHost,
{
    let layer = Arc::new(UdpSessionLayer::new_with_stun_responder(socket, host));
    let session = if classify_quic {
        layer.open_classified_session(UdpSessionProtocol::Quic, remote_addr)?
    } else {
        layer.connect(remote_addr).await?
    };
    Ok(ConnectedUdpSession::new(session, layer))
}
