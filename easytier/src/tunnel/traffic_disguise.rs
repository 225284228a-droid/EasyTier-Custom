//! Bounded, demand-driven pacing shared by HTTPS transports.

use easytier_core::{
    packet::{PacketType, ZCPacket},
    tunnel::TunnelError,
};
use futures::{Future, Sink};
use rand::Rng;
use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};
use tokio::time::{Instant, Sleep};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TrafficStyle {
    #[default]
    Speedtest,
    Video,
}

impl TrafficStyle {
    pub(crate) fn from_url(url: &url::Url) -> Self {
        match url.query_pairs().find(|(key, _)| key == "traffic_disguise") {
            Some((_, value)) if value == "video" => Self::Video,
            _ => Self::Speedtest,
        }
    }
}

/// Explicit per-URL policy always wins. Keep this separate from SNI policy.
pub(crate) fn apply_default(mut url: url::Url, configured: &str) -> url::Url {
    if !url.query_pairs().any(|(key, _)| key == "traffic_disguise") {
        url.query_pairs_mut().append_pair(
            "traffic_disguise",
            if configured == "video" {
                "video"
            } else {
                "speedtest"
            },
        );
    }
    url
}

fn paced_data(packet: &ZCPacket) -> bool {
    packet.peer_manager_header().is_some_and(|header| {
        !header.is_latency_first()
            && matches!(header.packet_type,
            kind if kind == PacketType::Data as u8
                || kind == PacketType::KcpSrc as u8 || kind == PacketType::KcpDst as u8
                || kind == PacketType::QuicSrc as u8 || kind == PacketType::QuicDst as u8
                || kind == PacketType::ParallelData as u8
                || kind == PacketType::ParallelLossyData as u8)
    })
}

pub(crate) struct PacedSink<S> {
    inner: S,
    style: TrafficStyle,
    pending: VecDeque<ZCPacket>,
    sleep: Option<Pin<Box<Sleep>>>,
    window_at: Instant,
    last_data: Instant,
    window_bytes: usize,
    burst_bytes: usize,
    burst_limit: usize,
}

impl<S> PacedSink<S> {
    pub(crate) fn new(inner: S, style: TrafficStyle) -> Self {
        Self {
            inner,
            style,
            pending: VecDeque::new(),
            sleep: None,
            window_at: Instant::now(),
            last_data: Instant::now(),
            window_bytes: 0,
            burst_bytes: 0,
            burst_limit: 64 * 1024,
        }
    }

    fn after_data(&mut self, bytes: usize) {
        let now = Instant::now();
        if now.duration_since(self.last_data) > Duration::from_millis(100)
            || now.duration_since(self.window_at) > Duration::from_millis(250)
        {
            self.window_at = now;
            self.window_bytes = 0;
            self.burst_bytes = 0;
        }
        self.last_data = now;
        self.window_bytes += bytes;
        self.burst_bytes += bytes;
        // Sparse traffic never waits, and no timer emits idle padding.
        if self.style == TrafficStyle::Video
            && self.window_bytes >= 256 * 1024
            && self.burst_bytes >= self.burst_limit
        {
            let mut rng = rand::thread_rng();
            self.burst_limit = rng.gen_range(32 * 1024..=128 * 1024);
            self.burst_bytes = 0;
            let pause = rng.gen_range(0..=5);
            self.sleep =
                (pause > 0).then(|| Box::pin(tokio::time::sleep(Duration::from_millis(pause))));
        }
    }
}

impl<S: Sink<ZCPacket, Error = TunnelError> + Unpin> PacedSink<S> {
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TunnelError>> {
        while let Some(packet) = self.pending.front() {
            let data = paced_data(packet);
            if data && let Some(sleep) = self.sleep.as_mut() {
                // Finish the previous burst before pausing the next one.
                ready!(Pin::new(&mut self.inner).poll_flush(cx))?;
                ready!(sleep.as_mut().poll(cx));
                self.sleep = None;
            }
            ready!(Pin::new(&mut self.inner).poll_ready(cx))?;
            let packet = self.pending.pop_front().unwrap();
            let bytes = packet.payload_len();
            Pin::new(&mut self.inner).start_send(packet)?;
            if data {
                self.after_data(bytes);
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: Sink<ZCPacket, Error = TunnelError> + Unpin> Sink<ZCPacket> for PacedSink<S> {
    type Error = TunnelError;
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.pending.len() >= 32 {
            ready!(self.drain(cx))?;
        }
        Poll::Ready(Ok(()))
    }
    fn start_send(mut self: Pin<&mut Self>, packet: ZCPacket) -> Result<(), Self::Error> {
        // Control/liveness traffic may bypass an outstanding video pause.
        if paced_data(&packet) {
            self.pending.push_back(packet);
        } else {
            let index = self
                .pending
                .iter()
                .position(paced_data)
                .unwrap_or(self.pending.len());
            self.pending.insert(index, packet);
        }
        Ok(())
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.drain(cx))?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.drain(cx))?;
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_url_style_wins() {
        let url = apply_default(
            "http3://localhost?traffic_disguise=video".parse().unwrap(),
            "speedtest",
        );
        assert_eq!(TrafficStyle::from_url(&url), TrafficStyle::Video);
        assert_eq!(
            TrafficStyle::from_url(&apply_default("wss://localhost".parse().unwrap(), "video")),
            TrafficStyle::Video
        );
    }
    #[tokio::test]
    async fn sparse_video_and_latency_sensitive_data_do_not_pause() {
        let mut sink = PacedSink::new((), TrafficStyle::Video);
        for _ in 0..8 {
            sink.after_data(1500);
        }
        assert!(sink.sleep.is_none());
        let mut packet = ZCPacket::new_with_payload(&[0; 1500]);
        packet.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        assert!(paced_data(&packet));
        packet
            .mut_peer_manager_header()
            .unwrap()
            .set_latency_first(true);
        assert!(!paced_data(&packet));
    }

    #[derive(Default)]
    struct RecordingSink(Vec<u8>);
    impl Sink<ZCPacket> for RecordingSink {
        type Error = TunnelError;
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), TunnelError>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(mut self: Pin<&mut Self>, packet: ZCPacket) -> Result<(), TunnelError> {
            self.0
                .push(packet.peer_manager_header().unwrap().packet_type);
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), TunnelError>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), TunnelError>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn controls_bypass_a_video_pause_and_speedtest_never_schedules_one() {
        let mut video = PacedSink::new(RecordingSink::default(), TrafficStyle::Video);
        video.sleep = Some(Box::pin(tokio::time::sleep(Duration::from_millis(200))));
        let mut data = ZCPacket::new_with_payload(b"data");
        data.fill_peer_manager_hdr(1, 2, PacketType::Data as u8);
        Pin::new(&mut video).start_send(data).unwrap();
        let mut ping = ZCPacket::new_with_payload(b"ping");
        ping.fill_peer_manager_hdr(1, 2, PacketType::Ping as u8);
        Pin::new(&mut video).start_send(ping).unwrap();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(video.drain(&mut cx).is_pending());
        assert_eq!(video.inner.0, [PacketType::Ping as u8]);
        let mut speedtest = PacedSink::new((), TrafficStyle::Speedtest);
        for _ in 0..1024 {
            speedtest.after_data(4096);
        }
        assert!(speedtest.sleep.is_none());
    }
}
