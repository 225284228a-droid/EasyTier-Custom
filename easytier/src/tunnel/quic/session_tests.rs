use std::{net::SocketAddr, time::Duration};

use easytier_core::{
    connectivity::transport::{ConnectedUdpSession, UdpSessionMode, connect_udp},
    packet::{PEER_MANAGER_HEADER_SIZE, ZCPacket},
    socket::{
        SocketListener,
        udp::{
            UdpBindOptions, UdpSessionAcceptKind, UdpSessionListenRequest, UdpSessionProtocol,
            VirtualUdpSocket,
        },
    },
    tunnel::{Tunnel, TunnelError},
};
use futures::{SinkExt, StreamExt};
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig, default_runtime};
use tokio::task::JoinHandle;

use crate::{
    common::netns::NetNS,
    host_runtime::native_host_runtime,
    socket::udp::{RuntimeUdpSessionSocketListener, new_runtime_udp_session_listener},
};

use super::{
    QuicAcceptedSession, client_config, endpoint_config, server_config,
    session_common::MAX_TUNNEL_BODY_SIZE, upgrade_connected, window_source,
};
use easytier_core::connectivity::protocol::ServerProtocolAdmissionController;

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
            UdpBindOptions::direct_connect()
                .with_local_addr(Some(bind_addr))
                .with_only_v6(false),
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

pub(crate) async fn assert_bidirectional_frame_boundaries(
    client: Box<dyn Tunnel>,
    server_task: JoinHandle<Box<dyn Tunnel>>,
) {
    let (mut client_read, mut client_write) = client.split();
    // QUIC does not expose an opened stream to the server until its first write.
    client_write
        .send(ZCPacket::new_with_payload(b"open framed stream"))
        .await
        .unwrap();
    let server = server_task.await.unwrap();
    let (mut server_read, mut server_write) = server.split();
    let opening = server_read.next().await.unwrap().unwrap();
    assert_eq!(opening.payload(), b"open framed stream");

    for body_size in [2001, MAX_TUNNEL_BODY_SIZE - 1, MAX_TUNNEL_BODY_SIZE] {
        let client_payload = vec![0x41; body_size - PEER_MANAGER_HEADER_SIZE];
        client_write
            .send(ZCPacket::new_with_payload(&client_payload))
            .await
            .unwrap();
        let received = server_read.next().await.unwrap().unwrap();
        assert_eq!(
            received.payload(),
            client_payload,
            "client body={body_size}"
        );

        let server_payload = vec![0x52; body_size - PEER_MANAGER_HEADER_SIZE];
        server_write
            .send(ZCPacket::new_with_payload(&server_payload))
            .await
            .unwrap();
        let received = client_read.next().await.unwrap().unwrap();
        assert_eq!(
            received.payload(),
            server_payload,
            "server body={body_size}"
        );
    }

    let oversized = vec![0x63; MAX_TUNNEL_BODY_SIZE + 1 - PEER_MANAGER_HEADER_SIZE];
    client_write
        .send(ZCPacket::new_with_payload(&oversized))
        .await
        .unwrap();
    server_write
        .send(ZCPacket::new_with_payload(&oversized))
        .await
        .unwrap();
    for received in [server_read.next().await, client_read.next().await] {
        assert!(matches!(
            received,
            Some(Err(TunnelError::InvalidPacket(message))) if message == "body too long"
        ));
    }
    assert!(server_read.next().await.is_none());
    assert!(client_read.next().await.is_none());
}

pub(crate) fn assert_bbr_controller(connection: &Connection) {
    let controller = connection.congestion_state().into_any();
    assert!(controller.is::<quinn::congestion::Bbr>());
}

pub(crate) async fn assert_endpoint_controllers(
    client_config: ClientConfig,
    server_config: ServerConfig,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let runtime = default_runtime().unwrap();
        let server_endpoint = Endpoint::new(
            endpoint_config(),
            Some(server_config),
            std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
            runtime.clone(),
        )
        .unwrap();
        let mut client_endpoint = Endpoint::new(
            endpoint_config(),
            None,
            std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
            runtime,
        )
        .unwrap();
        client_endpoint.set_default_client_config(client_config);
        let connecting = client_endpoint
            .connect(server_endpoint.local_addr().unwrap(), "localhost")
            .unwrap();
        let (client, server) = tokio::join!(connecting, async {
            server_endpoint.accept().await.unwrap().await.unwrap()
        });
        let client = client.unwrap();
        assert_bbr_controller(&client);
        assert_bbr_controller(&server);
        let client_source = window_source(&client);
        let server_source = window_source(&server);
        for source in [&client_source, &server_source] {
            let window = source
                .transmission_window()
                .expect("a newly established QUIC path must expose its sending window");
            assert_eq!(
                window.congestion_control,
                easytier_core::foundation::bandwidth::CongestionControl::Bbr,
            );
            assert!(window.congestion_window_bytes > 0);
            assert!(window.estimated_bps().is_some());
        }
        client_endpoint.close(0u32.into(), b"test done");
        server_endpoint.close(0u32.into(), b"test done");
        let _ = tokio::join!(client.closed(), server.closed());
        assert!(client_source.transmission_window().is_none());
        assert!(server_source.transmission_window().is_none());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn quic_endpoints_use_bbr() {
    assert_endpoint_controllers(client_config(), server_config()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn quic_tunnel_enforces_bidirectional_frame_boundary() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (mut listener, connected, _) = connected_udp_session_fixture("quic").await;
        let local_url = listener.local_url();
        let remote_url = local_url.clone();
        let server_task = tokio::spawn(async move {
            let session = listener.accept().await.unwrap();
            let admission = ServerProtocolAdmissionController::quic()
                .try_admit()
                .unwrap();
            let mut accepted = QuicAcceptedSession::new(session, local_url, admission).unwrap();
            accepted.accept().await.unwrap()
        });
        let client = upgrade_connected(connected, remote_url).await.unwrap();
        assert_bidirectional_frame_boundaries(client, server_task).await;
    })
    .await
    .unwrap();
}
