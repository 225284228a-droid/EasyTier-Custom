//! A UDP session that applies [`datagram_fec`](super::datagram_fec) to the
//! QUIC datagrams passing through it.
//!
//! It is transparent until the tunnel above has exchanged a key with the peer:
//! arming makes the receive side understand protected datagrams, and enabling
//! the send side follows once the peer is known to be armed.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use easytier_core::socket::udp::{UdpSessionKind, UdpSessionRecvMeta, UdpSessionSocket};
use tokio::sync::Notify;
use tokio_util::task::AbortOnDropHandle;

use super::datagram_fec::{
    Decoder, Encoder, FecKey, Kind, MAX_SOURCE_SIZE, RatioController, RatioMode,
};

const STATS_INTERVAL: Duration = Duration::from_secs(30);
const REPORT_POLL: Duration = Duration::from_millis(50);

/// What the local configuration asks of an HTTP3 tunnel's FEC.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FecConfig {
    /// Ask the peer for protection. A peer that asks gets it either way.
    pub wanted: bool,
    pub mode: RatioMode,
}

impl FecConfig {
    pub(crate) const OFF: Self = Self {
        wanted: false,
        mode: RatioMode::Adaptive,
    };

    /// The instance setting: whether FEC is requested, and the redundancy in
    /// percent, where zero follows the measured loss.
    pub(crate) fn from_flags(enabled: bool, percent: u32) -> Self {
        Self {
            wanted: enabled,
            mode: RatioMode::from_percent(percent),
        }
    }

    /// `fec=off`, `fec=auto` or `fec=<percent>` on a tunnel URL overrides the
    /// instance setting for that tunnel.
    pub(crate) fn with_url_override(self, url: &url::Url) -> Self {
        let Some((_, value)) = url.query_pairs().find(|(key, _)| key == "fec") else {
            return self;
        };
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "no" => Self::OFF,
            "auto" | "on" | "true" | "yes" | "" => Self {
                wanted: true,
                mode: RatioMode::Adaptive,
            },
            percent => match percent.trim_end_matches('%').parse::<u32>() {
                Ok(percent) => Self {
                    wanted: true,
                    mode: RatioMode::from_percent(percent),
                },
                Err(_) => self,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct FecStats {
    pub active: bool,
    pub tx_sources: u64,
    pub tx_repairs: u64,
    pub rx_sources: u64,
    pub rx_repairs: u64,
    pub recovered: u64,
    /// Repairs per source the sender currently adds.
    pub ratio: f32,
    /// Loss of the sending direction as the peer measured it.
    pub loss: Option<f32>,
    /// Loss the repairs did not mend.
    pub residual: Option<f32>,
}

struct Active {
    key: FecKey,
    decoder: Decoder,
    /// Present once the peer is known to understand protected datagrams.
    tx: Option<(Encoder, RatioController)>,
}

#[derive(Default)]
struct State {
    active: Option<Active>,
    recovered: VecDeque<Vec<u8>>,
    stats: FecStats,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Notify,
}

/// The tunnel's control over the session's FEC.
#[derive(Clone)]
pub(crate) struct FecHandle(Arc<Shared>);

impl FecHandle {
    /// A handle on no session, for tunnels built on a bare QUIC endpoint.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self(Arc::default())
    }
    /// Makes the receive side understand datagrams protected with `key`, and
    /// only those. The send side stays plain until [`enable_tx`].
    ///
    /// [`enable_tx`]: Self::enable_tx
    pub(crate) fn rekey(&self, key: [u8; 16]) {
        let mut state = self.0.state.lock().unwrap();
        let key = FecKey::new(key);
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.key == key)
        {
            return;
        }
        state.active = Some(Active {
            key,
            decoder: Decoder::new(key),
            tx: None,
        });
        state.recovered.clear();
        state.stats.active = false;
        drop(state);
        self.0.wake.notify_one();
    }

    /// Starts protecting what this end sends. Only after [`arm`](Self::arm),
    /// and only once the peer is armed with the same key.
    pub(crate) fn enable_tx(&self, mode: RatioMode) {
        let mut state = self.0.state.lock().unwrap();
        if let Some(active) = &mut state.active
            && active.tx.is_none()
        {
            active.tx = Some((Encoder::new(active.key), RatioController::new(mode)));
            state.stats.active = true;
            drop(state);
            self.0.wake.notify_one();
        }
    }

    /// Back to a plain session: the peer did not take the offer.
    pub(crate) fn disarm(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.active = None;
        state.recovered.clear();
        state.stats.active = false;
    }

    pub(crate) fn stats(&self) -> FecStats {
        let state = self.0.state.lock().unwrap();
        let mut stats = state.stats;
        if let Some((_, controller)) = state.active.as_ref().and_then(|active| active.tx.as_ref()) {
            stats.ratio = controller.ratio();
            stats.loss = controller.loss();
            stats.residual = controller.residual();
        }
        stats
    }
}

pub(crate) struct FecUdpSession {
    inner: Arc<dyn UdpSessionSocket>,
    shared: Arc<Shared>,
    _timer: AbortOnDropHandle<()>,
}

impl FecUdpSession {
    pub(crate) fn new(inner: Arc<dyn UdpSessionSocket>) -> (Arc<Self>, FecHandle) {
        let shared = Arc::new(Shared::default());
        let timer = AbortOnDropHandle::new(tokio::spawn(run_timer(inner.clone(), shared.clone())));
        (
            Arc::new(Self {
                inner,
                shared: shared.clone(),
                _timer: timer,
            }),
            FecHandle(shared),
        )
    }
}

/// Sends what no outgoing datagram triggers: the repair for a tail, and the
/// receiver's reports.
async fn run_timer(inner: Arc<dyn UdpSessionSocket>, shared: Arc<Shared>) {
    let mut last_stats = Instant::now();
    loop {
        let (due, deadline, armed) = {
            let mut state = shared.state.lock().unwrap();
            let now = Instant::now();
            let mut due = Vec::new();
            let mut deadline = None;
            let mut repairs = 0;
            if let Some(active) = &mut state.active {
                if let Some((encoder, controller)) = &mut active.tx {
                    encoder.tick(controller.ratio(), now, &mut due);
                    repairs = due.len() as u64;
                    deadline = encoder.deadline();
                    // The peer only learns the key's use from this end's
                    // datagrams, so reports wait for the send side as well.
                    due.extend(active.decoder.take_report(now));
                    deadline = [deadline, active.decoder.report_deadline(now)]
                        .into_iter()
                        .flatten()
                        .min();
                }
                active.decoder.expire(now);
            }
            state.stats.tx_repairs += repairs;
            (due, deadline, state.active.is_some())
        };
        for datagram in due {
            if inner.send(&datagram).await.is_err() {
                return;
            }
        }
        if last_stats.elapsed() >= STATS_INTERVAL {
            last_stats = Instant::now();
            let stats = FecHandle(shared.clone()).stats();
            if stats.active {
                tracing::debug!(?stats, peer = ?inner.peer_addr().ok(), "HTTP3 FEC");
            }
        }
        // Arrivals do not wake this task; it looks for a report to send at
        // least this often while the session is protected.
        let idle = if armed { REPORT_POLL } else { STATS_INTERVAL };
        let sleep = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(idle)
            .clamp(Duration::from_millis(1), idle);
        tokio::select! {
            () = tokio::time::sleep(sleep) => {}
            () = shared.wake.notified() => {}
        }
    }
}

#[async_trait]
impl UdpSessionSocket for FecUdpSession {
    fn kind(&self) -> UdpSessionKind {
        self.inner.kind()
    }

    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }

    fn peer_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.inner.peer_addr()
    }

    async fn send(&self, data: &[u8]) -> std::io::Result<usize> {
        let mut repairs = Vec::new();
        let source = {
            let mut state = self.shared.state.lock().unwrap();
            let state = &mut *state;
            match state.active.as_mut().and_then(|active| active.tx.as_mut()) {
                Some((encoder, controller)) if data.len() <= MAX_SOURCE_SIZE => {
                    let waiting = encoder.deadline().is_some();
                    let source =
                        encoder.source(data, controller.ratio(), Instant::now(), &mut repairs);
                    state.stats.tx_sources += 1;
                    state.stats.tx_repairs += repairs.len() as u64;
                    // Only a tail that starts now needs the timer's attention.
                    Some((source, !waiting && encoder.deadline().is_some()))
                }
                _ => None,
            }
        };
        let Some((source, tail)) = source else {
            return self.inner.send(data).await;
        };
        if self.inner.send(&source).await? != source.len() {
            return Ok(0);
        }
        for repair in repairs {
            // A repair is expendable; the source already left.
            if self.inner.send(&repair).await.is_err() {
                break;
            }
        }
        if tail {
            self.shared.wake.notify_one();
        }
        Ok(data.len())
    }

    async fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(self.recv_with_meta(buf).await?.0)
    }

    async fn recv_with_meta(&self, buf: &mut [u8]) -> std::io::Result<(usize, UdpSessionRecvMeta)> {
        loop {
            if let Some(datagram) = self.shared.state.lock().unwrap().recovered.pop_front() {
                if datagram.len() <= buf.len() {
                    buf[..datagram.len()].copy_from_slice(&datagram);
                    return Ok((datagram.len(), UdpSessionRecvMeta::default()));
                }
                continue;
            }
            let (len, meta) = self.inner.recv_with_meta(buf).await?;
            let mut state = self.shared.state.lock().unwrap();
            let state = &mut *state;
            let Some(active) = &mut state.active else {
                return Ok((len, meta));
            };
            let Some((kind, value, body_len)) = active.key.open(&buf[..len]) else {
                return Ok((len, meta));
            };
            let now = Instant::now();
            let body = &buf[..body_len];
            match kind {
                Kind::Source => {
                    state.stats.rx_sources += 1;
                    active.decoder.source(value, body, now);
                }
                Kind::Repair => {
                    state.stats.rx_repairs += 1;
                    active.decoder.repair(value, body, now);
                }
                Kind::Report => {
                    if let Some((_, controller)) = &mut active.tx
                        && let Some(report) = Decoder::parse_report(&active.key, value, body)
                    {
                        controller.on_report(report, now);
                    }
                }
            }
            while let Some(datagram) = active.decoder.take_recovered() {
                state.stats.recovered += 1;
                state.recovered.push_back(datagram);
            }
            if kind == Kind::Source {
                return Ok((body_len, meta));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::sync::mpsc;

    use super::*;

    /// Decides from the datagram's index and bytes whether the path loses it.
    type LossRule = Box<dyn Fn(u64, &[u8]) -> bool + Send + Sync>;

    /// One end of an in-memory datagram path that drops what `lose` selects.
    struct Path {
        tx: mpsc::UnboundedSender<Vec<u8>>,
        rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        lose: LossRule,
        sent: AtomicU64,
        wire_bytes: AtomicU64,
    }

    #[async_trait]
    impl UdpSessionSocket for Path {
        fn kind(&self) -> UdpSessionKind {
            UdpSessionKind::Quic
        }
        fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
            Ok("127.0.0.1:1".parse().unwrap())
        }
        fn peer_addr(&self) -> std::io::Result<std::net::SocketAddr> {
            Ok("127.0.0.1:2".parse().unwrap())
        }
        async fn send(&self, data: &[u8]) -> std::io::Result<usize> {
            let index = self.sent.fetch_add(1, Ordering::Relaxed);
            self.wire_bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
            if !(self.lose)(index, data) {
                let _ = self.tx.send(data.to_vec());
            }
            Ok(data.len())
        }
        async fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            let datagram = self
                .rx
                .lock()
                .await
                .recv()
                .await
                .ok_or(std::io::ErrorKind::UnexpectedEof)?;
            buf[..datagram.len()].copy_from_slice(&datagram);
            Ok(datagram.len())
        }
    }

    fn pair(
        lose_forward: impl Fn(u64, &[u8]) -> bool + Send + Sync + 'static,
    ) -> (Arc<Path>, Arc<Path>) {
        let (forward_tx, forward_rx) = mpsc::unbounded_channel();
        let (backward_tx, backward_rx) = mpsc::unbounded_channel();
        let end = |tx, rx, lose| {
            Arc::new(Path {
                tx,
                rx: tokio::sync::Mutex::new(rx),
                lose,
                sent: AtomicU64::new(0),
                wire_bytes: AtomicU64::new(0),
            })
        };
        (
            end(forward_tx, backward_rx, Box::new(lose_forward)),
            end(backward_tx, forward_rx, Box::new(|_, _| false)),
        )
    }

    fn datagram(index: u32) -> Vec<u8> {
        let mut datagram = vec![0x41; 200 + (index as usize * 13) % 900];
        datagram[1..5].copy_from_slice(&index.to_le_bytes());
        datagram
    }

    fn index_of(datagram: &[u8]) -> u32 {
        u32::from_le_bytes(datagram[1..5].try_into().unwrap())
    }

    /// Receives until `count` distinct datagrams arrived or the path goes
    /// quiet, checking every one against what was sent.
    async fn collect(session: &FecUdpSession, count: usize) -> std::collections::BTreeSet<u32> {
        let mut seen = std::collections::BTreeSet::new();
        let mut buf = vec![0; 2048];
        while seen.len() < count {
            let Ok(result) =
                tokio::time::timeout(Duration::from_millis(300), session.recv(&mut buf)).await
            else {
                break;
            };
            let len = result.unwrap();
            let index = index_of(&buf[..len]);
            assert_eq!(&buf[..len], datagram(index).as_slice());
            seen.insert(index);
        }
        seen
    }

    #[tokio::test]
    async fn an_unarmed_session_passes_datagrams_through_unchanged() {
        let (left, right) = pair(|_, _| false);
        let (left_fec, _) = FecUdpSession::new(left.clone());
        let (right_fec, handle) = FecUdpSession::new(right);
        for index in 0..20 {
            assert_eq!(
                left_fec.send(&datagram(index)).await.unwrap(),
                datagram(index).len()
            );
        }
        assert_eq!(collect(&right_fec, 20).await.len(), 20);
        assert_eq!(
            left.wire_bytes.load(Ordering::Relaxed),
            (0..20)
                .map(|index| datagram(index).len() as u64)
                .sum::<u64>()
        );
        assert!(!handle.stats().active);
    }

    #[tokio::test]
    async fn an_armed_receiver_accepts_plain_and_protected_datagrams_alike() {
        let (left, right) = pair(|_, _| false);
        let (left_fec, left_handle) = FecUdpSession::new(left);
        let (right_fec, right_handle) = FecUdpSession::new(right);
        // The offering end is armed before the answer arrives, while the
        // answering end may already protect what it sends.
        right_handle.rekey([9; 16]);
        left_fec.send(&datagram(0)).await.unwrap();
        left_handle.rekey([9; 16]);
        left_handle.enable_tx(RatioMode::Fixed(0.2));
        left_fec.send(&datagram(1)).await.unwrap();
        assert_eq!(collect(&right_fec, 2).await.len(), 2);
        assert_eq!(right_handle.stats().rx_sources, 1);
        assert!(left_handle.stats().active);

        // The same key again changes nothing.
        right_handle.rekey([9; 16]);
        left_fec.send(&datagram(2)).await.unwrap();
        assert_eq!(collect(&right_fec, 1).await.len(), 1);
        assert_eq!(right_handle.stats().rx_sources, 2);

        // A reconnect brings another key; the old one is no longer read, and
        // its datagrams reach QUIC as they are, which then discards them.
        right_handle.rekey([8; 16]);
        left_fec.send(&datagram(3)).await.unwrap();
        let mut buf = vec![0; 2048];
        let len = right_fec.recv(&mut buf).await.unwrap();
        assert_eq!(
            len,
            datagram(3).len() + super::super::datagram_fec::TRAILER_SIZE
        );
        assert_eq!(right_handle.stats().rx_sources, 2);

        // A refused offer leaves a plain session behind.
        left_handle.disarm();
        assert!(!left_handle.stats().active);
        left_fec.send(&datagram(4)).await.unwrap();
        assert_eq!(collect(&right_fec, 1).await.len(), 1);
    }

    #[tokio::test]
    async fn fixed_ratio_recovers_losses_and_costs_what_it_says() {
        // Every tenth datagram on the wire is lost, source or repair.
        let (left, right) = pair(|index, _| index % 10 == 7);
        let (left_fec, left_handle) = FecUdpSession::new(left.clone());
        let (right_fec, right_handle) = FecUdpSession::new(right);
        for handle in [&left_handle, &right_handle] {
            handle.rekey([3; 16]);
            handle.enable_tx(RatioMode::Fixed(0.25));
        }
        let total = 2000;
        let receiver = tokio::spawn(async move {
            let seen = collect(&right_fec, total).await;
            (seen, right_fec)
        });
        for index in 0..total as u32 {
            left_fec.send(&datagram(index)).await.unwrap();
            if index % 50 == 0 {
                tokio::task::yield_now().await;
            }
        }
        let (seen, _right_fec) = receiver.await.unwrap();
        assert!(seen.len() >= total - 4, "delivered {}", seen.len());
        let stats = left_handle.stats();
        assert_eq!(stats.tx_sources, total as u64);
        assert!((499..=501).contains(&stats.tx_repairs), "{stats:?}");
        let received = right_handle.stats();
        assert!(received.recovered >= 150, "{received:?}");
    }

    #[tokio::test]
    async fn adaptive_ratio_stays_idle_on_a_clean_path_and_engages_under_loss() {
        let lossy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lose = lossy.clone();
        let (left, right) = pair(move |index, _| lose.load(Ordering::Relaxed) && index % 12 == 5);
        let (left_fec, left_handle) = FecUdpSession::new(left.clone());
        let (right_fec, right_handle) = FecUdpSession::new(right);
        for handle in [&left_handle, &right_handle] {
            handle.rekey([5; 16]);
            handle.enable_tx(RatioMode::Adaptive);
        }
        let drain = {
            let right_fec = right_fec.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 2048];
                while right_fec.recv(&mut buf).await.is_ok() {}
            })
        };
        // Reports travel back to the sender, which has to read them.
        let reports = {
            let left_fec = left_fec.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 2048];
                while left_fec.recv(&mut buf).await.is_ok() {}
            })
        };
        let mut index = 0;
        let mut send = async |count: u32| {
            for _ in 0..count {
                left_fec.send(&datagram(index)).await.unwrap();
                index += 1;
                if index % 20 == 0 {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        };

        send(3000).await;
        let clean = left_handle.stats();
        assert_eq!(clean.ratio, 0.0, "{clean:?}");
        // Only the floor before the first measurement.
        assert!(clean.tx_repairs * 50 < clean.tx_sources, "{clean:?}");
        assert!(clean.loss.unwrap() < 0.001, "{clean:?}");

        lossy.store(true, Ordering::Relaxed);
        send(6000).await;
        let engaged = left_handle.stats();
        let loss = engaged.loss.unwrap();
        assert!((0.06..0.11).contains(&loss), "{engaged:?}");
        assert!((0.1..0.5).contains(&engaged.ratio), "{engaged:?}");
        assert!(engaged.tx_repairs > 300, "{engaged:?}");
        assert!(right_handle.stats().recovered > 200);
        drain.abort();
        reports.abort();
    }

    #[test]
    fn url_overrides_the_instance_setting() {
        let on = FecConfig {
            wanted: true,
            mode: RatioMode::Fixed(0.2),
        };
        let parse = |config: FecConfig, query: &str| {
            config.with_url_override(&format!("http3://host:1{query}").parse().unwrap())
        };
        assert_eq!(parse(on, ""), on);
        assert_eq!(parse(on, "?sni=a"), on);
        assert_eq!(parse(on, "?fec=off"), FecConfig::OFF);
        assert_eq!(parse(on, "?fec=nonsense"), on);
        assert_eq!(parse(FecConfig::OFF, "?fec=auto").mode, RatioMode::Adaptive);
        assert!(parse(FecConfig::OFF, "?fec=auto").wanted);
        assert_eq!(
            parse(FecConfig::OFF, "?fec=35").mode,
            RatioMode::Fixed(0.35)
        );
        assert_eq!(
            parse(FecConfig::OFF, "?fec=35%25").mode,
            RatioMode::Fixed(0.35)
        );
        assert_eq!(parse(FecConfig::OFF, "?fec=0").mode, RatioMode::Adaptive);
    }

    struct Transfer {
        lost_packets: u64,
        sent_packets: u64,
        recovered: u64,
        elapsed: Duration,
    }

    /// Sends `bytes` over a real HTTP3-configured QUIC connection (BBR and the
    /// tunnel's loss detection) across a 20 ms RTT link that loses `loss` of
    /// the forward datagrams.
    async fn quic_transfer(fec: Option<RatioMode>, loss: f64, bytes: usize) -> Transfer {
        use crate::tunnel::{http3, quic::session_common::build_session_endpoint};

        let server_addr: std::net::SocketAddr = super::super::test_link::SERVER.parse().unwrap();
        let (client_link, server_link) =
            super::super::test_link::pair(loss, Duration::from_millis(10));
        let (client_session, client_fec) = FecUdpSession::new(client_link);
        let (server_session, server_fec) = FecUdpSession::new(server_link);
        if let Some(mode) = fec {
            for handle in [&client_fec, &server_fec] {
                handle.rekey([0x5a; 16]);
                handle.enable_tx(mode);
            }
        }
        let socket = |session: Arc<FecUdpSession>| {
            Arc::new(
                super::super::QuicUdpSessionSocket::from_session(session, Box::new(())).unwrap(),
            )
        };
        let server = build_session_endpoint(
            socket(server_session),
            Some(http3::server_config_for(fec.is_some()).unwrap()),
            http3::endpoint_config(),
        )
        .unwrap();
        let mut client =
            build_session_endpoint(socket(client_session), None, http3::endpoint_config()).unwrap();
        client.set_default_client_config(http3::client_config_for(fec.is_some()).unwrap());

        let receiver = tokio::spawn(async move {
            let connection = server.accept().await.unwrap().await.unwrap();
            let (_, mut recv) = connection.accept_bi().await.unwrap();
            let received = recv.read_to_end(usize::MAX).await.unwrap();
            (received.len(), connection, server)
        });
        let connection = client
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .unwrap();
        let started = Instant::now();
        let (mut send, _recv) = connection.open_bi().await.unwrap();
        let chunk = vec![0x33; 64 * 1024];
        for _ in 0..bytes / chunk.len() {
            send.write_all(&chunk).await.unwrap();
        }
        send.finish().unwrap();
        let (received, _server_connection, _server) = receiver.await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(received, bytes / chunk.len() * chunk.len());
        let stats = connection.stats();
        Transfer {
            lost_packets: stats.path.lost_packets,
            sent_packets: stats.path.sent_packets,
            recovered: server_fec.stats().recovered,
            elapsed,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repairs_arrive_before_quic_declares_the_loss() {
        tokio::time::timeout(Duration::from_secs(120), async {
            let bytes = 3 * 1024 * 1024;
            let plain = quic_transfer(None, 0.05, bytes).await;
            let protected = quic_transfer(Some(RatioMode::Fixed(0.25)), 0.05, bytes).await;
            // Adaptive mode first has to measure the loss.
            let adaptive = quic_transfer(Some(RatioMode::Adaptive), 0.05, 4 * bytes).await;
            for (name, transfer) in [
                ("plain", &plain),
                ("fixed 25%", &protected),
                ("adaptive", &adaptive),
            ] {
                eprintln!(
                    "{name}: {:?}, QUIC lost {} of {} sent, FEC recovered {}",
                    transfer.elapsed,
                    transfer.lost_packets,
                    transfer.sent_packets,
                    transfer.recovered
                );
            }
            // Without FEC QUIC itself sees the 5% loss.
            assert!(plain.lost_packets * 40 > plain.sent_packets);
            // With it, almost every loss is mended below QUIC in time, so
            // QUIC neither retransmits nor enters recovery for it.
            assert!(protected.recovered > 0);
            assert!(protected.lost_packets * 5 < plain.lost_packets);
            let rate =
                |transfer: &Transfer| transfer.lost_packets as f64 / transfer.sent_packets as f64;
            assert!(rate(&adaptive) * 3.0 < rate(&plain));
        })
        .await
        .unwrap();
    }
}
