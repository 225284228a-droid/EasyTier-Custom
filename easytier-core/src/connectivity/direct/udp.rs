use std::{net::SocketAddr, sync::Arc};

use url::Url;

use crate::{
    connectivity::protocol::{ProtocolTransport, protocol_transport},
    connectivity::transport::{ConnectedUdpSession, UdpSessionMode},
    socket::udp::{UdpSessionLayer, VirtualUdpSocketFactory},
};

use super::DirectConnectorHost;

pub(super) async fn connect_with_socket<H>(
    host: Arc<H>,
    socket: Arc<<H as VirtualUdpSocketFactory>::Socket>,
    remote_addr: SocketAddr,
    url: &Url,
) -> anyhow::Result<ConnectedUdpSession>
where
    H: DirectConnectorHost,
{
    let mode = match protocol_transport(url.scheme()) {
        Some(ProtocolTransport::Udp(mode)) => mode,
        _ => anyhow::bail!("UDP punch requires a UDP listener: {url}"),
    };
    let layer = Arc::new(UdpSessionLayer::new_with_stun_responder(socket, host));
    let session = match mode {
        UdpSessionMode::Classified(protocol) => {
            layer.open_classified_session(protocol, remote_addr)?
        }
        UdpSessionMode::EasyTierMux => layer.connect(remote_addr).await?,
    };
    Ok(ConnectedUdpSession::new(session, layer))
}
