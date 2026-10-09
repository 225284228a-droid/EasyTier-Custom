use std::{net::SocketAddr, time::Duration};

use easytier_core::{
    connectivity::{
        protocol::ServerTunnelAcceptor,
        transport::{ConnectedUdpSession, UdpSessionMode, connect_udp},
    },
    packet::ZCPacket,
    socket::{
        SocketListener,
        udp::{
            UdpBindOptions, UdpSessionAcceptKind, UdpSessionListenRequest, UdpSessionProtocol,
            VirtualUdpSocket,
        },
    },
    tunnel::{
        Tunnel,
        framed::{FramedReader, FramedWriter},
    },
};
use futures::{SinkExt, StreamExt};
use quinn::Endpoint;
use tokio::task::JoinHandle;

use crate::{
    common::netns::NetNS,
    host_runtime::native_host_runtime,
    socket::udp::{RuntimeUdpSessionSocketListener, new_runtime_udp_session_listener},
    tunnel::common::tests::_tunnel_echo_server,
};

pub(crate) async fn connected_udp_session_fixture(
    scheme: &str,
) -> (
    RuntimeUdpSessionSocketListener,
    ConnectedUdpSession,
    SocketAddr,
) {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut listener = new_runtime_udp_session_listener(
        format!("{scheme}://{bind_addr}").parse().unwrap(),
        UdpSessionListenRequest::new(
            UdpBindOptions::port_bound_listener(bind_addr).with_only_v6(false),
        ),
        UdpSessionAcceptKind::Classified(UdpSessionProtocol::Quic),
        NetNS::new(None),
    );
    listener.listen().await.unwrap();
    let remote_addr = listener.bound_socket().unwrap().local_addr().unwrap();
    let connected = connect_udp(
        native_host_runtime(),
        remote_addr,
        Vec::new(),
        UdpBindOptions::direct_connect(),
        UdpSessionMode::Classified(UdpSessionProtocol::Quic),
    )
    .await
    .unwrap();
    (listener, connected, remote_addr)
}

pub(crate) async fn accept_two_connections_on_same_session(
    accepted: &mut dyn ServerTunnelAcceptor,
    listener: &mut RuntimeUdpSessionSocketListener,
) -> (Box<dyn Tunnel>, Box<dyn Tunnel>) {
    let first = accepted.accept().await.unwrap();
    let second = accepted.accept().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "both {} connections must use the first accepted UDP session",
        listener.local_url().scheme(),
    );
    (first, second)
}

pub(crate) async fn assert_second_connection_survives_first_close(
    endpoint: Endpoint,
    remote_addr: SocketAddr,
    server_task: JoinHandle<(Box<dyn Tunnel>, Box<dyn Tunnel>)>,
    protocol_name: &str,
) {
    let first_connection = endpoint
        .connect(remote_addr, "localhost")
        .unwrap()
        .await
        .unwrap();
    super::tests::assert_bbr_controller(&first_connection);
    let (first_write, first_read) = first_connection.open_bi().await.unwrap();
    let mut first_send = FramedWriter::new(first_write);
    first_send
        .send(ZCPacket::new_with_payload(
            format!("first {protocol_name} connection").as_bytes(),
        ))
        .await
        .unwrap();
    let second_connection = endpoint
        .connect(remote_addr, "localhost")
        .unwrap()
        .await
        .unwrap();
    let (second_write, second_read) = second_connection.open_bi().await.unwrap();
    let mut second_send = FramedWriter::new(second_write);
    let ready_payload = format!("second {protocol_name} connection ready");
    second_send
        .send(ZCPacket::new_with_payload(ready_payload.as_bytes()))
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
    assert_eq!(ready.payload(), ready_payload.as_bytes());
    let after_close_payload = format!("second {protocol_name} connection after first closed");
    second_send
        .send(ZCPacket::new_with_payload(after_close_payload.as_bytes()))
        .await
        .unwrap();
    let packet = recv.next().await.unwrap().unwrap();
    assert_eq!(packet.payload(), after_close_payload.as_bytes());
    let _ = second_send.close().await;
    echo_task.await.unwrap();
    second_connection.close(0u32.into(), b"second connection done");
    endpoint.close(0u32.into(), b"test done");
}
