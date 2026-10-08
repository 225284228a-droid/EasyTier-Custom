use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use easytier_core::foundation::bandwidth::{TransmissionWindow, TransmissionWindowSource};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

#[derive(Debug)]
struct Sample {
    next_at: Instant,
    observed_at: Option<Instant>,
    window: Option<TransmissionWindow>,
    stopped: bool,
}

#[derive(Debug)]
pub(super) struct TcpWindowSampler(Mutex<Sample>);

impl TcpWindowSampler {
    pub(super) fn new() -> Self {
        Self(Mutex::new(Sample {
            next_at: Instant::now(),
            observed_at: None,
            window: None,
            stopped: false,
        }))
    }

    pub(super) fn sample(&self, stream: &TcpStream) {
        let now = Instant::now();
        let mut sample = self.0.lock().unwrap();
        if sample.stopped || now < sample.next_at {
            return;
        }
        sample.next_at = now + Duration::from_millis(200);
        sample.record_window(now, native_window(stream));
    }

    pub(super) fn stop(&self) {
        let mut sample = self.0.lock().unwrap();
        sample.stopped = true;
        sample.window = None;
    }
}

impl Sample {
    fn record_window(&mut self, now: Instant, window: Option<TransmissionWindow>) {
        // OS telemetry can be temporarily unavailable or uninitialized between
        // I/O polls. Only valid samples refresh the existing sample's lifetime.
        if let Some(window) = window
            && window.estimated_bps().is_some()
        {
            self.observed_at = Some(now);
            self.window = Some(window);
        }
    }
}

#[derive(Debug)]
pub(super) struct TcpWindowSource(pub(super) Weak<TcpWindowSampler>);

impl TransmissionWindowSource for TcpWindowSource {
    fn transmission_window(&self) -> Option<TransmissionWindow> {
        let sampler = self.0.upgrade()?;
        let sample = sampler.0.lock().unwrap();
        if sample.stopped || sample.observed_at?.elapsed() >= Duration::from_secs(60) {
            return None;
        }
        sample.window
    }
}

// Sample while a borrowed live stream owns its handle. Telemetry never duplicates
// a socket or retains a raw descriptor after its I/O halves have been dropped.
pub(super) struct TcpWindowIo<T> {
    inner: T,
    sampler: Arc<TcpWindowSampler>,
}

impl<T> TcpWindowIo<T> {
    pub(super) fn new(inner: T, sampler: Arc<TcpWindowSampler>) -> Self {
        Self { inner, sampler }
    }
}

impl<T: AsyncRead + AsRef<TcpStream> + Unpin> AsyncRead for TcpWindowIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.sampler.sample(this.inner.as_ref());
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + AsRef<TcpStream> + Unpin> AsyncWrite for TcpWindowIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.sampler.sample(this.inner.as_ref());
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.sampler.sample(this.inner.as_ref());
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_shutdown(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            this.sampler.stop();
        }
        result
    }
}

#[cfg(target_os = "linux")]
fn native_window(stream: &TcpStream) -> Option<TransmissionWindow> {
    use std::{
        mem::{offset_of, size_of},
        os::fd::AsRawFd,
    };
    let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::tcp_info>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            (&mut info as *mut libc::tcp_info).cast(),
            &mut len,
        )
    };
    if result != 0 || (len as usize) < offset_of!(libc::tcp_info, tcpi_snd_cwnd) + size_of::<u32>()
    {
        return None;
    }
    let peer_receive_window_bytes = ((len as usize)
        >= offset_of!(libc::tcp_info, tcpi_snd_wnd) + size_of::<u32>())
    .then_some(u64::from(info.tcpi_snd_wnd));
    Some(TransmissionWindow {
        congestion_window_bytes: u64::from(info.tcpi_snd_cwnd) * u64::from(info.tcpi_snd_mss),
        peer_receive_window_bytes,
        rtt: Duration::from_micros(u64::from(info.tcpi_rtt)),
    })
}

#[cfg(windows)]
fn native_window(stream: &TcpStream) -> Option<TransmissionWindow> {
    use std::{
        mem::{offset_of, size_of},
        os::windows::io::AsRawSocket,
    };
    use windows::Win32::Networking::WinSock::{SIO_TCP_INFO, SOCKET, TCP_INFO_v0, WSAIoctl};
    let version = 0_u32;
    let mut info = TCP_INFO_v0::default();
    let mut len = 0_u32;
    let result = unsafe {
        WSAIoctl(
            SOCKET(stream.as_raw_socket() as usize),
            SIO_TCP_INFO,
            Some((&version as *const u32).cast()),
            size_of::<u32>() as u32,
            Some((&mut info as *mut TCP_INFO_v0).cast()),
            size_of::<TCP_INFO_v0>() as u32,
            &mut len,
            None,
            None,
        )
    };
    if result != 0 || (len as usize) < offset_of!(TCP_INFO_v0, SndWnd) + size_of::<u32>() {
        return None;
    }
    Some(TransmissionWindow {
        congestion_window_bytes: u64::from(info.Cwnd),
        peer_receive_window_bytes: Some(u64::from(info.SndWnd)),
        rtt: Duration::from_micros(u64::from(info.RttUs)),
    })
}

#[cfg(not(any(target_os = "linux", windows)))]
fn native_window(_stream: &TcpStream) -> Option<TransmissionWindow> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn a_real_tcp_socket_exposes_window_telemetry_across_its_owned_split() {
        use easytier_core::socket::tcp::VirtualTcpSocket;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let socket = super::super::RuntimeTcpSocket::new(client);
        let source = socket.bandwidth_source().unwrap();
        let (mut reader, mut writer) = socket.into_split();
        let echo = tokio::spawn(async move {
            let mut bytes = [0; 4];
            peer.read_exact(&mut bytes).await.unwrap();
            peer.write_all(&bytes).await.unwrap();
            peer
        });
        tokio::time::sleep(Duration::from_millis(220)).await;
        writer.write_all(b"ping").await.unwrap();
        let mut bytes = [0; 4];
        reader.read_exact(&mut bytes).await.unwrap();
        let _peer = echo.await.unwrap();
        let window = source
            .transmission_window()
            .expect("the OS did not report a TCP window");
        assert!(window.congestion_window_bytes > 0);
        assert!(window.peer_receive_window_bytes.is_some());
        assert!(window.estimated_bps().is_some());
        drop(reader);
        drop(writer);
        assert!(source.transmission_window().is_none());
    }

    #[test]
    fn telemetry_does_not_keep_an_io_owner_alive() {
        let sampler = Arc::new(TcpWindowSampler::new());
        let source = TcpWindowSource(Arc::downgrade(&sampler));
        drop(sampler);
        assert!(source.transmission_window().is_none());
    }

    #[test]
    fn closed_or_expired_samples_are_not_reported() {
        let sampler = Arc::new(TcpWindowSampler::new());
        let source = TcpWindowSource(Arc::downgrade(&sampler));
        {
            let mut sample = sampler.0.lock().unwrap();
            sample.observed_at = Some(Instant::now());
            sample.window = Some(TransmissionWindow {
                congestion_window_bytes: 125_000,
                peer_receive_window_bytes: Some(250_000),
                rtt: Duration::from_millis(10),
            });
        }
        assert_eq!(
            source.transmission_window().unwrap().estimated_bps(),
            Some(100_000_000)
        );
        sampler.0.lock().unwrap().observed_at = Some(Instant::now() - Duration::from_secs(61));
        assert!(source.transmission_window().is_none());
        sampler.0.lock().unwrap().observed_at = Some(Instant::now());
        sampler.stop();
        assert!(source.transmission_window().is_none());
    }

    #[test]
    fn transient_missing_or_invalid_tcp_samples_keep_the_last_valid_window() {
        let sampler = Arc::new(TcpWindowSampler::new());
        let source = TcpWindowSource(Arc::downgrade(&sampler));
        let now = Instant::now();
        let window = TransmissionWindow {
            congestion_window_bytes: 125_000,
            peer_receive_window_bytes: Some(250_000),
            rtt: Duration::from_millis(10),
        };
        {
            let mut sample = sampler.0.lock().unwrap();
            sample.record_window(now, Some(window));
            sample.record_window(now + Duration::from_secs(1), None);
            sample.record_window(
                now + Duration::from_secs(2),
                Some(TransmissionWindow {
                    rtt: Duration::ZERO,
                    ..window
                }),
            );
            sample.record_window(
                now + Duration::from_secs(3),
                Some(TransmissionWindow {
                    peer_receive_window_bytes: Some(0),
                    ..window
                }),
            );
            assert_eq!(sample.observed_at, Some(now));
        }
        assert_eq!(source.transmission_window(), Some(window));
        let recovered = TransmissionWindow {
            rtt: Duration::from_millis(20),
            ..window
        };
        sampler
            .0
            .lock()
            .unwrap()
            .record_window(now + Duration::from_secs(4), Some(recovered));
        assert_eq!(source.transmission_window(), Some(recovered));
        assert_eq!(
            sampler.0.lock().unwrap().observed_at,
            Some(now + Duration::from_secs(4)),
        );
        sampler.0.lock().unwrap().observed_at = Some(now - Duration::from_secs(61));
        assert!(source.transmission_window().is_none());
    }

    #[test]
    fn missing_first_tcp_sample_is_unavailable_until_a_valid_sample_arrives() {
        let sampler = Arc::new(TcpWindowSampler::new());
        let source = TcpWindowSource(Arc::downgrade(&sampler));
        sampler
            .0
            .lock()
            .unwrap()
            .record_window(Instant::now(), None);
        assert!(source.transmission_window().is_none());
    }
}
