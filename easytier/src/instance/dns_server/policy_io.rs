//! User DNS forwarding follows the current packet policy. Bootstrap DNS uses
//! the separate protected native resolver in common::dns.
use crate::socket_protector::ProtectedSocket;
use easytier_core::{
    gateway::{DataPlaneTcpStream, DataPlaneUdpSocket},
    instance::CorePacketPlane,
    peers::vpn_policy::VpnDecision,
    socket::{SocketContext, tcp::TcpBindOptions, udp::UdpBindOptions},
};
use hickory_proto::{
    runtime::{RuntimeProvider, TokioRuntimeProvider, TokioTime, iocompat::AsyncIoTokioAsStd},
    udp::DnsUdpSocket,
};
use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpStream, UdpSocket},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Egress {
    Native,
    Overlay,
    Block,
}
type Policy = Arc<dyn Fn(&IpAddr) -> Egress + Send + Sync>;

fn policy_for(plane: Arc<CorePacketPlane>) -> Policy {
    Arc::new(move |address| match plane.classify_vpn(address) {
        VpnDecision::Block => Egress::Block,
        VpnDecision::Exit(_) => Egress::Overlay,
        VpnDecision::Direct => Egress::Native,
        VpnDecision::Passthrough if plane.vpn_overlay_destination(address) => Egress::Overlay,
        VpnDecision::Passthrough => Egress::Native,
    })
}

fn ensure_policy(policy: &Policy, address: SocketAddr, expected: Egress) -> io::Result<()> {
    let current = policy(&address.ip());
    if current == Egress::Block || current != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "DNS forwarding policy changed or blocks this destination",
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct PolicyDnsIo {
    runtime: TokioRuntimeProvider,
    plane: Arc<CorePacketPlane>,
    context: SocketContext,
    policy: Policy,
}

impl PolicyDnsIo {
    pub(super) fn new(plane: Arc<CorePacketPlane>, context: SocketContext) -> Self {
        Self {
            runtime: TokioRuntimeProvider::new(),
            policy: policy_for(plane.clone()),
            plane,
            context,
        }
    }
}

impl RuntimeProvider for PolicyDnsIo {
    type Handle = <TokioRuntimeProvider as RuntimeProvider>::Handle;
    type Timer = TokioTime;
    type Udp = PolicyUdp;
    type Tcp = AsyncIoTokioAsStd<PolicyTcp>;
    fn create_handle(&self) -> Self::Handle {
        self.runtime.create_handle()
    }

    fn connect_tcp(
        &self,
        server: SocketAddr,
        bind: Option<SocketAddr>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn Future<Output = io::Result<Self::Tcp>> + Send>> {
        let plane = self.plane.clone();
        let context = self.context.clone();
        let policy = self.policy.clone();
        Box::pin(async move {
            let timeout = timeout.unwrap_or(Duration::from_secs(5));
            let expected = policy(&server.ip());
            let transport = match expected {
                Egress::Native => {
                    let options = TcpBindOptions::default()
                        .with_context(context)
                        .with_local_addr(bind)
                        .with_bind_device(Some(String::new()))
                        .with_reuse_addr(false);
                    let connect = async {
                        crate::socket::tcp::create_tcp_socket(server, &options)
                            .await
                            .map_err(io::Error::other)?
                            .connect(server)
                            .await
                    };
                    TcpTransport::Native(tokio::time::timeout(timeout, connect).await.map_err(
                        |_| {
                            io::Error::new(
                                io::ErrorKind::TimedOut,
                                "native DNS TCP connect timed out",
                            )
                        },
                    )??)
                }
                Egress::Overlay => {
                    TcpTransport::Overlay(plane.vpn_dns_tcp_connect(server, timeout).await?)
                }
                Egress::Block => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "DNS destination blocked",
                    ));
                }
            };
            ensure_policy(&policy, server, expected)?;
            Ok(AsyncIoTokioAsStd(PolicyTcp {
                transport: Mutex::new(transport),
                policy,
                peer: server,
                expected,
            }))
        })
    }

    fn bind_udp(
        &self,
        local: SocketAddr,
        server: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<Self::Udp>> + Send>> {
        let plane = self.plane.clone();
        let context = self.context.clone();
        let policy = self.policy.clone();
        Box::pin(async move {
            let expected = policy(&server.ip());
            let transport = match expected {
                Egress::Native => UdpTransport::Native(
                    crate::socket::udp::create_udp_socket(
                        &UdpBindOptions::default()
                            .with_context(context)
                            .with_local_addr(Some(local)),
                    )
                    .await
                    .map_err(io::Error::other)?,
                ),
                Egress::Overlay => UdpTransport::Overlay(Arc::new(
                    plane
                        .vpn_dns_udp_bind(local, Duration::from_secs(5))
                        .await?,
                )),
                Egress::Block => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "DNS destination blocked",
                    ));
                }
            };
            ensure_policy(&policy, server, expected)?;
            Ok(PolicyUdp {
                transport,
                policy,
                peer: server,
                expected,
                send: Mutex::new(None),
                receive: Mutex::new(None),
            })
        })
    }
}

enum TcpTransport {
    Native(ProtectedSocket<TcpStream>),
    Overlay(DataPlaneTcpStream),
}
pub(super) struct PolicyTcp {
    transport: Mutex<TcpTransport>,
    policy: Policy,
    peer: SocketAddr,
    expected: Egress,
}
impl AsyncRead for PolicyTcp {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Err(error) = ensure_policy(&self.policy, self.peer, self.expected) {
            return Poll::Ready(Err(error));
        }
        match self.transport.get_mut().unwrap() {
            TcpTransport::Native(stream) => Pin::new(stream).poll_read(cx, buf),
            TcpTransport::Overlay(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}
impl AsyncWrite for PolicyTcp {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Err(error) = ensure_policy(&self.policy, self.peer, self.expected) {
            return Poll::Ready(Err(error));
        }
        match self.transport.get_mut().unwrap() {
            TcpTransport::Native(stream) => Pin::new(stream).poll_write(cx, buf),
            TcpTransport::Overlay(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = ensure_policy(&self.policy, self.peer, self.expected) {
            return Poll::Ready(Err(error));
        }
        match self.transport.get_mut().unwrap() {
            TcpTransport::Native(stream) => Pin::new(stream).poll_flush(cx),
            TcpTransport::Overlay(stream) => Pin::new(stream).poll_flush(cx),
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.transport.get_mut().unwrap() {
            TcpTransport::Native(stream) => Pin::new(stream).poll_shutdown(cx),
            TcpTransport::Overlay(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

enum UdpTransport {
    Native(ProtectedSocket<UdpSocket>),
    Overlay(Arc<DataPlaneUdpSocket>),
}
type SendFuture = Pin<Box<dyn Future<Output = io::Result<usize>> + Send>>;
type ReceiveFuture = Pin<Box<dyn Future<Output = io::Result<(Vec<u8>, SocketAddr)>> + Send>>;
struct PendingSend {
    target: SocketAddr,
    bytes: Vec<u8>,
    future: SendFuture,
}
pub(super) struct PolicyUdp {
    transport: UdpTransport,
    policy: Policy,
    peer: SocketAddr,
    expected: Egress,
    send: Mutex<Option<PendingSend>>,
    receive: Mutex<Option<ReceiveFuture>>,
}

#[async_trait::async_trait]
impl DnsUdpSocket for PolicyUdp {
    type Time = TokioTime;
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        bytes: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        if let Err(error) = ensure_policy(&self.policy, target, self.expected) {
            self.send.lock().unwrap().take();
            return Poll::Ready(Err(error));
        }
        match &self.transport {
            UdpTransport::Native(socket) => DnsUdpSocket::poll_send_to(socket, cx, bytes, target),
            UdpTransport::Overlay(socket) => {
                let mut pending = self.send.lock().unwrap();
                if pending
                    .as_ref()
                    .is_none_or(|send| send.target != target || send.bytes != bytes)
                {
                    let socket = socket.clone();
                    let owned = bytes.to_vec();
                    let data = owned.clone();
                    *pending = Some(PendingSend {
                        target,
                        bytes: owned,
                        future: Box::pin(async move { socket.send_to(&data, target).await }),
                    });
                }
                let result = pending.as_mut().unwrap().future.as_mut().poll(cx);
                if result.is_ready() {
                    pending.take();
                }
                result
            }
        }
    }

    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        if let Err(error) = ensure_policy(&self.policy, self.peer, self.expected) {
            self.receive.lock().unwrap().take();
            return Poll::Ready(Err(error));
        }
        match &self.transport {
            UdpTransport::Native(socket) => DnsUdpSocket::poll_recv_from(socket, cx, bytes),
            UdpTransport::Overlay(socket) => {
                let mut pending = self.receive.lock().unwrap();
                if pending.is_none() {
                    let socket = socket.clone();
                    *pending = Some(Box::pin(async move {
                        let mut buffer = vec![0; 65535];
                        let (length, peer) = socket.recv_from(&mut buffer).await?;
                        buffer.truncate(length);
                        Ok((buffer, peer))
                    }));
                }
                match pending.as_mut().unwrap().as_mut().poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        pending.take();
                        Poll::Ready(result.map(|(buffer, peer)| {
                            let length = bytes.len().min(buffer.len());
                            bytes[..length].copy_from_slice(&buffer[..length]);
                            (length, peer)
                        }))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn cached_native_dns_connection_cannot_send_after_exit_or_block_patch() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = listener.local_addr().unwrap();
        let client = TcpStream::connect(peer).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        let state = Arc::new(AtomicU8::new(0));
        let view = state.clone();
        let policy: Policy = Arc::new(move |_| match view.load(Ordering::SeqCst) {
            0 => Egress::Native,
            1 => Egress::Overlay,
            _ => Egress::Block,
        });
        let mut cached = PolicyTcp {
            transport: Mutex::new(TcpTransport::Native(ProtectedSocket::new(client, None))),
            policy,
            peer,
            expected: Egress::Native,
        };
        cached.write_all(b"direct").await.unwrap();
        let mut received = [0; 6];
        server.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"direct");
        for changed in [1, 2] {
            state.store(changed, Ordering::SeqCst);
            assert_eq!(
                cached.write_all(b"must not leak").await.unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(20), server.read(&mut received))
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn cached_native_udp_dns_socket_cannot_send_after_exit_or_block_patch() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = server.local_addr().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let state = Arc::new(AtomicU8::new(0));
        let view = state.clone();
        let policy: Policy = Arc::new(move |_| match view.load(Ordering::SeqCst) {
            0 => Egress::Native,
            1 => Egress::Overlay,
            _ => Egress::Block,
        });
        let cached = PolicyUdp {
            transport: UdpTransport::Native(ProtectedSocket::new(socket, None)),
            policy,
            peer,
            expected: Egress::Native,
            send: Mutex::new(None),
            receive: Mutex::new(None),
        };
        DnsUdpSocket::send_to(&cached, b"direct", peer)
            .await
            .unwrap();
        let mut received = [0; 16];
        assert_eq!(server.recv_from(&mut received).await.unwrap().0, 6);
        for changed in [1, 2] {
            state.store(changed, Ordering::SeqCst);
            assert_eq!(
                DnsUdpSocket::send_to(&cached, b"must not leak", peer)
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(20), server.recv_from(&mut received))
                    .await
                    .is_err()
            );
        }
    }
}
