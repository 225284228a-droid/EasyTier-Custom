use std::{
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use easytier_core::{
    gateway::proxy::icmp_host::{IcmpProxyHost, IcmpProxySocket, ProxyRuntimeError},
    socket::SocketContext,
};
use socket2::Socket;

use crate::common::netns::NetNS;
use crate::socket_protector::SocketRegistration;

#[derive(Debug, Default)]
pub(crate) struct RuntimeIcmpProxyHost;

impl RuntimeIcmpProxyHost {
    // Mirrors tunnel::common::bind: create and mark the socket inside the
    // namespace guard, then register with the native protection system outside
    // it so the guard never crosses an await.
    async fn create_raw_socket(
        context: &SocketContext,
    ) -> std::io::Result<(Socket, Option<Arc<SocketRegistration>>)> {
        let socket = {
            let _guard = NetNS::from_socket_context(context).guard();
            let socket = Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::RAW,
                Some(socket2::Protocol::ICMPV4),
            )?;
            // Apply the configured mark first; a platform protector or an
            // active lease ORs its bypass bit on top of it afterwards.
            crate::tunnel::common::apply_socket_mark(&socket, context.socket_mark)
                .map_err(std::io::Error::other)?;
            socket.bind(&socket2::SockAddr::from(SocketAddrV4::new(
                Ipv4Addr::UNSPECIFIED,
                0,
            )))?;
            socket
        };
        let registration = crate::socket_protector::protect_native_socket(&socket, true).await?;
        Ok((socket, registration))
    }
}

#[derive(Debug)]
struct RuntimeIcmpSocket {
    socket: Arc<Socket>,
    registration: Option<Arc<SocketRegistration>>,
    closed: Arc<AtomicBool>,
}

impl RuntimeIcmpSocket {
    fn new(socket: Socket, registration: Option<Arc<SocketRegistration>>) -> std::io::Result<Self> {
        // shutdown() does not reliably interrupt recv_from() on an unconnected
        // raw socket (notably on Windows). Bound each blocking read so close
        // also releases Tokio's blocking worker when no ICMP traffic arrives.
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok(Self {
            socket: Arc::new(socket),
            registration,
            closed: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl Drop for RuntimeIcmpSocket {
    fn drop(&mut self) {
        self.close();
    }
}

#[async_trait::async_trait]
impl IcmpProxySocket for RuntimeIcmpSocket {
    async fn send(&self, destination: Ipv4Addr, packet: &[u8]) -> Result<(), ProxyRuntimeError> {
        let remote = SocketAddrV4::new(destination, 0);
        // Scope the datagram through the protection system so an active VPN
        // lease steers it out a physical interface instead of the captured
        // route. Linux is a passthrough here: its bypass mark is persistent.
        crate::socket_protector::with_udp_scope(
            self.registration.as_ref(),
            &self.socket,
            None,
            remote.into(),
            || {
                self.socket
                    .send_to(packet, &socket2::SockAddr::from(remote))
            },
        )??;
        Ok(())
    }

    async fn recv(&self) -> Result<(IpAddr, Vec<u8>), ProxyRuntimeError> {
        let socket = self.socket.clone();
        let closed = self.closed.clone();
        tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0_u8; 8192];
            loop {
                if closed.load(Ordering::Acquire) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "ICMP socket closed",
                    )
                    .into());
                }
                let uninitialized: &mut [MaybeUninit<u8>] =
                    unsafe { std::mem::transmute(&mut buffer[..]) };
                match socket_recv(&socket, uninitialized) {
                    Ok((length, peer_ip)) if !closed.load(Ordering::Acquire) => {
                        buffer.truncate(length);
                        return Ok((peer_ip, buffer));
                    }
                    Ok(_) => continue,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        })
        .await
        .map_err(|error| ProxyRuntimeError::Other(error.into()))?
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

#[async_trait::async_trait]
impl IcmpProxyHost for RuntimeIcmpProxyHost {
    async fn open_icmp_v4(
        &self,
        context: SocketContext,
    ) -> Result<Arc<dyn IcmpProxySocket>, ProxyRuntimeError> {
        let (socket, registration) =
            Self::create_raw_socket(&context)
                .await
                .inspect_err(|error| {
                    tracing::warn!(?error, "create ICMP socket failed");
                })?;
        Ok(Arc::new(RuntimeIcmpSocket::new(socket, registration)?))
    }
}

fn socket_recv(
    socket: &Socket,
    buffer: &mut [MaybeUninit<u8>],
) -> Result<(usize, IpAddr), std::io::Error> {
    let (size, address) = socket.recv_from(buffer)?;
    let peer_ip = address
        .as_socket()
        .map(|address| address.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    Ok((size, peer_ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    // UDP exercises the same unconnected blocking recv_from/close behavior
    // without requiring raw-socket privileges or modifying host routes.
    fn socket() -> Arc<RuntimeIcmpSocket> {
        Arc::new(
            RuntimeIcmpSocket::new(
                std::net::UdpSocket::bind("127.0.0.1:0").unwrap().into(),
                None,
            )
            .unwrap(),
        )
    }

    #[test]
    fn close_releases_pending_receive_without_traffic() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let socket = socket();
        let result = runtime.block_on(async {
            let receiving = socket.clone();
            let task = tokio::spawn(async move { receiving.recv().await });
            tokio::time::sleep(Duration::from_millis(25)).await;
            socket.close();
            tokio::time::timeout(Duration::from_secs(2), task).await
        });
        // Keep the regression test bounded even if a blocking worker leaks.
        runtime.shutdown_timeout(Duration::from_secs(2));
        assert!(
            result
                .expect("receive must finish after close")
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn create_raw_socket_registers_with_native_protection() {
        let (socket, registration) =
            match RuntimeIcmpProxyHost::create_raw_socket(&SocketContext::default()).await {
                Ok(created) => created,
                // Raw ICMP sockets require privileges on most platforms.
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    return;
                }
                Err(error) => panic!("unexpected ICMP socket creation failure: {error}"),
            };
        if crate::socket_protector::native_socket_protection_available() {
            assert!(registration.is_some(), "ICMP socket must be registered");
        }
        drop(socket);
    }
}
