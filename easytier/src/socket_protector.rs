use std::{
    io,
    ops::Deref,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};

use async_trait::async_trait;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;

mod desktop;
#[cfg(not(target_os = "linux"))]
pub(crate) use desktop::interface_index;
pub(crate) use desktop::{DesktopProtectionLease, SocketRegistration, namespace_for};
#[cfg(target_os = "linux")]
pub(crate) use desktop::{active_bit, choose_bypass_bit, used_socket_marks};

/// Keeps the protection registration alive for exactly the native socket's
/// lifetime, including both halves of an owned TCP split.
#[derive(Debug)]
pub struct ProtectedSocket<T> {
    inner: T,
    registration: Option<Arc<SocketRegistration>>,
}

impl<T> ProtectedSocket<T> {
    pub(crate) fn new(inner: T, registration: Option<Arc<SocketRegistration>>) -> Self {
        Self {
            inner,
            registration,
        }
    }

    pub(crate) fn parts(self) -> (T, Option<Arc<SocketRegistration>>) {
        (self.inner, self.registration)
    }
}

impl<T> Deref for ProtectedSocket<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}
impl<T, U: ?Sized> AsRef<U> for ProtectedSocket<T>
where
    T: AsRef<U>,
{
    fn as_ref(&self) -> &U {
        self.inner.as_ref()
    }
}

#[cfg(unix)]
impl<T: AsRawFd> AsRawFd for ProtectedSocket<T> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.inner.as_raw_fd()
    }
}
#[cfg(windows)]
impl<T: AsRawSocket> AsRawSocket for ProtectedSocket<T> {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        self.inner.as_raw_socket()
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for ProtectedSocket<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for ProtectedSocket<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl ProtectedSocket<tokio::net::TcpSocket> {
    pub(crate) fn listen(
        self,
        backlog: u32,
    ) -> io::Result<ProtectedSocket<tokio::net::TcpListener>> {
        let (socket, registration) = self.parts();
        Ok(ProtectedSocket::new(socket.listen(backlog)?, registration))
    }
    pub(crate) async fn connect(
        self,
        remote: std::net::SocketAddr,
    ) -> io::Result<ProtectedSocket<tokio::net::TcpStream>> {
        let (socket, registration) = self.parts();
        if registration.is_some() {
            desktop::prepare_tcp_connect(&socket2::SockRef::from(&socket), remote)?;
        }
        let stream = socket.connect(remote).await?;
        // Connected sockets now have their actual source address, which lets
        // native scope protection respect non-default physical interfaces.
        if registration.is_some() {
            desktop::refresh_scope(&socket2::SockRef::from(&stream))?;
        }
        Ok(ProtectedSocket::new(stream, registration))
    }
}

impl ProtectedSocket<tokio::net::UdpSocket> {
    pub(crate) async fn connect<A: tokio::net::ToSocketAddrs>(&self, remote: A) -> io::Result<()> {
        let remote = tokio::net::lookup_host(remote)
            .await?
            .next()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::AddrNotAvailable, "no UDP peer address")
            })?;
        let socket = socket2::SockRef::from(&self.inner);
        desktop::with_udp_scope(self.registration.as_ref(), &socket, None, remote, || {
            socket.connect(&remote.into())
        })?
    }

    pub(crate) fn try_send_to(
        &self,
        data: &[u8],
        remote: std::net::SocketAddr,
    ) -> io::Result<usize> {
        desktop::with_udp_scope(
            self.registration.as_ref(),
            &socket2::SockRef::from(&self.inner),
            None,
            remote,
            || self.inner.try_send_to(data, remote),
        )?
    }

    pub(crate) async fn send_to(
        &self,
        data: &[u8],
        remote: std::net::SocketAddr,
    ) -> io::Result<usize> {
        self.inner
            .async_io(tokio::io::Interest::WRITABLE, || {
                self.try_send_to(data, remote)
            })
            .await
    }

    pub(crate) async fn send(&self, data: &[u8]) -> io::Result<usize> {
        let remote = self.inner.peer_addr()?;
        self.inner
            .async_io(tokio::io::Interest::WRITABLE, || {
                desktop::with_udp_scope(
                    self.registration.as_ref(),
                    &socket2::SockRef::from(&self.inner),
                    None,
                    remote,
                    || self.inner.try_send(data),
                )?
            })
            .await
    }

    pub(crate) async fn send_to_with_src_ip(
        &self,
        source: std::net::IpAddr,
        remote: std::net::SocketAddr,
        data: &[u8],
    ) -> io::Result<usize> {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            self.inner
                .async_io(tokio::io::Interest::WRITABLE, || {
                    desktop::with_udp_scope(
                        self.registration.as_ref(),
                        &socket2::SockRef::from(&self.inner),
                        Some(source),
                        remote,
                        || {
                            crate::socket::udp_src::send_to_with_src_ip_raw(
                                &self.inner,
                                source,
                                remote,
                                data,
                            )
                        },
                    )?
                })
                .await
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        crate::socket::udp_src::send_to_with_src_ip(&self.inner, source, remote, data).await
    }

    pub(crate) fn send_to_with_src_ipv6(
        &self,
        source: std::net::Ipv6Addr,
        ifindex: u32,
        remote: std::net::SocketAddrV6,
        data: &[u8],
    ) -> io::Result<usize> {
        desktop::with_udp_scope(
            self.registration.as_ref(),
            &socket2::SockRef::from(&self.inner),
            Some(source.into()),
            remote.into(),
            || {
                crate::socket::udp_src::send_to_with_src_ipv6(
                    &self.inner,
                    source,
                    ifindex,
                    remote,
                    data,
                )
            },
        )?
    }
}

#[cfg(feature = "dns-resolver")]
#[async_trait]
impl hickory_proto::udp::DnsUdpSocket for ProtectedSocket<tokio::net::UdpSocket> {
    type Time = hickory_proto::runtime::TokioTime;
    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, std::net::SocketAddr)>> {
        let mut read = tokio::io::ReadBuf::new(buf);
        self.inner
            .poll_recv_from(cx, &mut read)
            .map(|r| r.map(|addr| (read.filled().len(), addr)))
    }
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: std::net::SocketAddr,
    ) -> Poll<io::Result<usize>> {
        match desktop::with_udp_scope(
            self.registration.as_ref(),
            &socket2::SockRef::from(&self.inner),
            None,
            target,
            || self.inner.poll_send_to(cx, buf, target),
        ) {
            Ok(result) => result,
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

#[cfg(feature = "upnp")]
#[async_trait]
impl natpmp::AsyncUdpSocket for ProtectedSocket<tokio::net::UdpSocket> {
    async fn connect(&self, addr: &str) -> io::Result<()> {
        self.connect(addr).await
    }
    async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.send(buf).await
    }
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.recv(buf).await
    }
}

/// Native-only platform callback used by socket creation when `need_protect`
/// is set. The future must resolve only after protection is actually applied;
/// emitting an event without awaiting its acknowledgement is not sufficient.
/// Errors fail creation before bind/connect/listen or exposing an accepted child.
///
/// WASI embedders implement the same contract inside their existing host socket
/// creation operations, using core's encoded bind options, not this raw-FD API.
#[async_trait]
pub trait NativeSocketProtector: Send + Sync + 'static {
    async fn protect(&self, socket_handle: u64) -> io::Result<()>;
}

static NATIVE_SOCKET_PROTECTOR: RwLock<Option<Arc<dyn NativeSocketProtector>>> = RwLock::new(None);

/// Installs or removes the process-wide native socket protection capability.
///
/// Instance-specific routing policy still travels in socket options; this hook
/// only exposes a platform service such as Android/iOS/HarmonyOS VPN bypass.
pub fn set_native_socket_protector(protector: Option<Arc<dyn NativeSocketProtector>>) {
    let mut guard = NATIVE_SOCKET_PROTECTOR
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = protector;
}

fn native_socket_protector() -> Option<Arc<dyn NativeSocketProtector>> {
    #[cfg(all(test, unix))]
    if let Ok(protector) = TEST_SOCKET_PROTECTOR.try_with(Arc::clone) {
        return Some(protector);
    }
    NATIVE_SOCKET_PROTECTOR
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

// Isolate factory ordering tests from other tests' sockets in the same process.
#[cfg(all(test, unix))]
tokio::task_local! {
    static TEST_SOCKET_PROTECTOR: Arc<dyn NativeSocketProtector>;
}

pub(crate) fn native_socket_protection_available() -> bool {
    native_socket_protector().is_some()
        || (cfg!(feature = "dns-resolver") && desktop::available())
        || desktop::active()
}

// socket2 owns the platform-specific handle; SockRef adapts Tokio sockets here.
pub(crate) async fn protect_native_socket(
    socket: &socket2::Socket,
    need_protect: bool,
) -> io::Result<Option<Arc<SocketRegistration>>> {
    if !need_protect {
        return Ok(None);
    }
    let registration = desktop::register(socket)?;
    let Some(protector) = native_socket_protector() else {
        return Ok(registration);
    };
    #[cfg(unix)]
    let handle = u64::try_from(socket.as_raw_fd())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket fd"))?;
    #[cfg(windows)]
    let handle = socket.as_raw_socket();
    protector.protect(handle).await?;
    // An external protector may update SO_MARK during its acknowledged
    // callback. Reapply only our reserved bypass bit while preserving it.
    desktop::refresh_scope(socket)?;
    Ok(registration)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use easytier_core::socket::{
        SocketListener,
        tcp::{TcpBindOptions, TcpConnectOptions, TcpListenOptions, VirtualTcpListener},
        udp::UdpBindOptions,
    };
    use std::{
        os::fd::BorrowedFd,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::sync::Semaphore;

    struct GateProtector {
        calls: AtomicUsize,
        gate: Semaphore,
        fail: bool,
        expect_unbound: bool,
    }

    impl GateProtector {
        fn new(fail: bool, expect_unbound: bool) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                gate: Semaphore::new(0),
                fail,
                expect_unbound,
            })
        }
    }

    #[async_trait]
    impl NativeSocketProtector for GateProtector {
        async fn protect(&self, handle: u64) -> io::Result<()> {
            // The caller keeps the socket alive while this borrowed callback runs.
            let fd = unsafe { BorrowedFd::borrow_raw(i32::try_from(handle).unwrap()) };
            let socket = socket2::SockRef::from(&fd);
            if self.expect_unbound || self.calls.load(Ordering::SeqCst) == 0 {
                assert_eq!(socket.local_addr()?.as_socket().unwrap().port(), 0);
                assert!(
                    socket.peer_addr().is_err(),
                    "connect must wait for protect completion"
                );
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "test protection failure",
                ));
            }
            self.gate.acquire().await.unwrap().forget();
            Ok(())
        }
    }

    #[tokio::test]
    async fn tcp_connect_waits_for_protection_ack() {
        let protector = GateProtector::new(false, true);
        TEST_SOCKET_PROTECTOR
            .scope(protector.clone(), async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let options = TcpConnectOptions::direct_connect(listener.local_addr().unwrap());
                let connect = crate::socket::tcp::connect_tcp(options);
                tokio::pin!(connect);
                assert!(futures::poll!(&mut connect).is_pending());
                assert_eq!(protector.calls.load(Ordering::SeqCst), 1);
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), listener.accept())
                        .await
                        .is_err()
                );
                protector.gate.add_permits(1);
                let _client = connect.await.unwrap();
                listener.accept().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn protection_failure_blocks_creation_and_explicit_false_bypasses_callback() {
        let protector = GateProtector::new(true, true);
        TEST_SOCKET_PROTECTOR
            .scope(protector.clone(), async {
                let local = "127.0.0.1:0".parse().unwrap();
                let bind = TcpBindOptions::default().with_local_addr(Some(local));
                assert!(
                    crate::socket::tcp::create_tcp_socket(local, &bind)
                        .await
                        .is_err()
                );
                assert!(
                    crate::socket::tcp::bind_tcp_listener(TcpListenOptions::direct_connect(local))
                        .await
                        .is_err()
                );
                let udp = UdpBindOptions::direct_connect().with_local_addr(Some(local));
                assert!(crate::socket::udp::create_udp_socket(&udp).await.is_err());
                assert_eq!(protector.calls.load(Ordering::SeqCst), 3);

                for options in [
                    TcpListenOptions::proxy_nat(local),
                    TcpListenOptions::socks5(local),
                    TcpListenOptions::port_forward(local),
                    TcpListenOptions::port_lease(local),
                ] {
                    let listener = crate::socket::tcp::bind_tcp_listener(options)
                        .await
                        .unwrap();
                    let connect = TcpConnectOptions::direct_connect(listener.local_addr().unwrap())
                        .with_bind(TcpBindOptions::default().with_need_protect(false));
                    let _client = crate::socket::tcp::connect_tcp(connect).await.unwrap();
                    listener.accept().await.unwrap();
                }
                let udp = udp.with_need_protect(false);
                crate::socket::udp::create_udp_socket(&udp).await.unwrap();
                let mut rpc = crate::proto::rpc::standalone::runtime_rpc_listener(local);
                rpc.listen().await.unwrap();
                let rpc_addr = rpc.local_url().socket_addrs(|| None).unwrap()[0];
                let _client = tokio::net::TcpStream::connect(rpc_addr).await.unwrap();
                rpc.accept().await.unwrap();
                assert_eq!(protector.calls.load(Ordering::SeqCst), 3);
            })
            .await;
    }

    #[tokio::test]
    async fn accepted_child_waits_for_inherited_protection() {
        let protector = GateProtector::new(false, false);
        TEST_SOCKET_PROTECTOR
            .scope(protector.clone(), async {
                let bind = crate::socket::tcp::bind_tcp_listener(TcpListenOptions::direct_connect(
                    "127.0.0.1:0".parse().unwrap(),
                ));
                tokio::pin!(bind);
                assert!(futures::poll!(&mut bind).is_pending());
                assert_eq!(protector.calls.load(Ordering::SeqCst), 1);
                protector.gate.add_permits(1);
                let listener = bind.await.unwrap();
                let _client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                    .await
                    .unwrap();
                let accept = listener.accept();
                tokio::pin!(accept);
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), &mut accept)
                        .await
                        .is_err()
                );
                assert_eq!(protector.calls.load(Ordering::SeqCst), 2);
                protector.gate.add_permits(1);
                accept.await.unwrap();
            })
            .await;
    }
}
