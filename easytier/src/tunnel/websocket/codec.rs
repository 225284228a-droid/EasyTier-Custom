//! WebSocket tunnel frame encoding and the versioned text-padding capability.

use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};

use bytes::BytesMut;
use easytier_core::{
    packet::{ZCPacket, ZCPacketType},
    tunnel::TunnelError,
};
use futures::Sink;
use tokio_websockets::Message;
use zerocopy::AsBytes as _;

use super::websocket_error;

pub(super) const TRANSPORT_FEATURES_HEADER: &str = "x-easytier-transport-features";
pub(super) const WS_TEXT_PADDING_V1: &str = "ws-text-padding-v1";

pub(super) fn supports_text_padding(headers: &http::HeaderMap) -> bool {
    headers
        .get_all(TRANSPORT_FEATURES_HEADER)
        .iter()
        .any(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .any(|feature| feature.trim() == WS_TEXT_PADDING_V1)
            })
        })
}

/// Padding frames are carried as WebSocket **text** messages so they can never
/// collide with real ZCPacket payloads, which are always binary. A real
/// ZCPacket over a ws/wss tunnel starts with its `PeerManagerHeader`, whose
/// first byte is the low byte of `from_peer_id` (little-endian) and can legally
/// be `0x00`, so no in-band binary marker could disambiguate padding from real
/// traffic. Text frames are out-of-band at the WebSocket layer and are simply
/// dropped on receive.
pub(super) struct WebSocketPacketSink<S> {
    inner: S,
    padding: Option<WebSocketPadding>,
    /// Frames already accepted via `start_send` (real packets and any due
    /// padding) but not yet handed to `inner`. `poll_ready` / `poll_flush` /
    /// `poll_close` only drain this buffer; they never produce new frames, so
    /// the `Sink` contract (poll_ready is side-effect-free beyond readiness)
    /// is honored. Padding is generated in `start_send` instead.
    pending: VecDeque<Message>,
}

struct WebSocketPadding {
    interval: Duration,
    size: usize,
    next_at: std::time::Instant,
}

impl<S> WebSocketPacketSink<S> {
    pub(super) fn new(inner: S) -> Self {
        Self {
            inner,
            padding: None,
            pending: VecDeque::new(),
        }
    }

    pub(super) fn with_padding(inner: S, interval: Duration, size: usize) -> Self {
        Self {
            inner,
            padding: Some(WebSocketPadding {
                interval,
                size,
                next_at: std::time::Instant::now(),
            }),
            pending: VecDeque::new(),
        }
    }

    fn pending_padding(&self) -> bool {
        self.padding
            .as_ref()
            .is_some_and(|padding| std::time::Instant::now() >= padding.next_at)
    }

    fn take_padding_frame(&mut self) -> Message {
        let padding = self.padding.as_mut().expect("padding checked before");
        let mut payload = vec![0u8; padding.size.max(2)];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut payload);
        padding.next_at = std::time::Instant::now() + padding.interval;
        // Base64 keeps the random payload valid UTF-8 so it can be sent as a
        // WebSocket text frame; text frames are out-of-band from the binary
        // wire format of real ZCPackets, so padding can never collide with
        // legitimate traffic.
        use base64::{Engine as _, prelude::BASE64_STANDARD};
        Message::text(BASE64_STANDARD.encode(payload))
    }

    /// Drain previously-accepted frames from `pending` into `inner`. This is
    /// the only side effect `poll_ready` / `poll_flush` / `poll_close` perform:
    /// pushing already-accepted items downstream, exactly like
    /// `futures::sink::Buffer`. No new frames are produced here.
    fn drain_pending<E>(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TunnelError>>
    where
        S: Sink<Message, Error = E> + Unpin,
        E: std::fmt::Display,
    {
        loop {
            if self.pending.is_empty() {
                return Poll::Ready(Ok(()));
            }
            ready!(
                Pin::new(&mut self.inner)
                    .poll_ready(cx)
                    .map_err(websocket_error)
            )?;
            let frame = self.pending.pop_front().expect("pending was non-empty");
            Pin::new(&mut self.inner)
                .start_send(frame)
                .map_err(websocket_error)?;
        }
    }
}

impl<S, E> Sink<ZCPacket> for WebSocketPacketSink<S>
where
    S: Sink<Message, Error = E> + Unpin,
    E: std::fmt::Display,
{
    type Error = TunnelError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Drain previously-accepted frames first. No new frames are generated
        // here, so `poll_ready` is free of the side effect that previously
        // violated the `Sink` contract.
        ready!(self.drain_pending(cx))?;
        Pin::new(&mut self.inner)
            .poll_ready(cx)
            .map(|result| result.map_err(websocket_error))
    }

    fn start_send(mut self: Pin<&mut Self>, packet: ZCPacket) -> Result<(), Self::Error> {
        // Keep the official zero-copy fast path when padding is disabled. The
        // queue is only needed when an optional disguise frame must precede
        // the packet and the inner sink may accept one frame per readiness
        // notification.
        if self.padding.is_none() && self.pending.is_empty() {
            return Pin::new(&mut self.inner)
                .start_send(Message::binary(packet.tunnel_payload_bytes().freeze()))
                .map_err(websocket_error);
        }
        // Padding is generated here, in `start_send` - a method explicitly
        // allowed to begin sending. The padding frame is enqueued ahead of the
        // real packet so the next `poll_ready` drains them in wire order
        // (padding, then packet), matching the pre-bug behavior. Both items go
        // through `pending` so we never rely on `inner` being ready for two
        // `start_send` calls (the trait only guarantees one slot per
        // `poll_ready`).
        if self.pending_padding() {
            let frame = self.take_padding_frame();
            self.pending.push_back(frame);
        }
        self.pending
            .push_back(Message::binary(packet.tunnel_payload_bytes().freeze()));
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.drain_pending(cx))?;
        Pin::new(&mut self.inner)
            .poll_flush(cx)
            .map(|result| result.map_err(websocket_error))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.drain_pending(cx))?;
        Pin::new(&mut self.inner)
            .poll_close(cx)
            .map(|result| result.map_err(websocket_error))
    }
}

pub(super) async fn map_from_ws_message(
    message: Result<Message, tokio_websockets::Error>,
    padding_expected: bool,
) -> Option<Result<ZCPacket, TunnelError>> {
    let message = match message {
        Ok(message) => message,
        Err(error) => {
            tracing::error!(?error, "recv from websocket error");
            return Some(Err(websocket_error(error)));
        }
    };
    if message.is_close() {
        tracing::warn!("recv close message from websocket");
        return None;
    }
    if message.is_text() {
        if padding_expected {
            tracing::trace!("dropping websocket text padding frame");
            return None;
        } else {
            return Some(Err(TunnelError::InvalidPacket(
                "unnegotiated websocket text frame".to_owned(),
            )));
        }
    }
    if !message.is_binary() {
        let message = format!("{message:?}");
        tracing::error!(?message, "Invalid packet");
        return Some(Err(TunnelError::InvalidPacket(message)));
    }
    let payload = message.into_payload();
    Some(Ok(ZCPacket::new_from_buf(
        BytesMut::from(payload.as_bytes()),
        ZCPacketType::DummyTunnel,
    )))
}

#[cfg(test)]
mod tests {
    use std::io;

    use futures::SinkExt;

    use super::*;

    #[test]
    fn text_padding_feature_requires_an_exact_versioned_token() {
        for (value, expected) in [
            ("", false),
            ("ws-text-padding-v2", false),
            ("prefix-ws-text-padding-v1", false),
            ("WS-TEXT-PADDING-V1", false),
            ("ws-text-padding-v1", true),
            ("other-v2, ws-text-padding-v1, future-v1", true),
        ] {
            let mut headers = http::HeaderMap::new();
            headers.insert(TRANSPORT_FEATURES_HEADER, value.parse().unwrap());
            assert_eq!(supports_text_padding(&headers), expected, "{value}");
        }
        assert!(!supports_text_padding(&http::HeaderMap::new()));
        let mut headers = http::HeaderMap::new();
        headers.append(TRANSPORT_FEATURES_HEADER, "other-v1".parse().unwrap());
        headers.append(
            TRANSPORT_FEATURES_HEADER,
            WS_TEXT_PADDING_V1.parse().unwrap(),
        );
        assert!(supports_text_padding(&headers));
    }

    struct FailingWebSocketSink {
        close_called: bool,
    }

    #[derive(Default)]
    struct RecordingWebSocketSink {
        messages: Vec<Message>,
    }

    impl Sink<Message> for RecordingWebSocketSink {
        type Error = io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            self.messages.push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Default)]
    struct SingleSlotWebSocketSink {
        messages: Vec<Message>,
        ready: bool,
        must_yield: bool,
    }

    impl Sink<Message> for SingleSlotWebSocketSink {
        type Error = io::Error;

        fn poll_ready(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            if self.must_yield {
                self.must_yield = false;
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                self.ready = true;
                Poll::Ready(Ok(()))
            }
        }

        fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            if !self.ready {
                return Err(io::Error::other("start_send needs a new readiness slot"));
            }
            self.ready = false;
            self.must_yield = true;
            self.messages.push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn packet_sink_padding_is_data_triggered_and_respects_backpressure() {
        let mut sink = WebSocketPacketSink::with_padding(
            SingleSlotWebSocketSink::default(),
            Duration::from_secs(60),
            64,
        );
        sink.flush().await.unwrap();
        futures::future::poll_fn(|cx| Pin::new(&mut sink).poll_ready(cx))
            .await
            .unwrap();
        assert!(sink.inner.messages.is_empty(), "idle polling must not pad");

        sink.send(ZCPacket::new_with_payload(b"backpressure probe"))
            .await
            .unwrap();
        assert!(sink.pending.is_empty());
        assert_eq!(sink.inner.messages.len(), 2);
        assert!(sink.inner.messages[0].is_text());
        assert!(sink.inner.messages[1].is_binary());
        sink.flush().await.unwrap();
        sink.close().await.unwrap();
        assert_eq!(sink.inner.messages.len(), 2);
    }

    impl Sink<Message> for FailingWebSocketSink {
        type Error = io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "send failed",
            )))
        }

        fn poll_close(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.close_called = true;
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "close failed",
            )))
        }
    }

    #[tokio::test]
    async fn packet_sink_maps_send_and_close_errors_independently() {
        let mut sink = WebSocketPacketSink::new(FailingWebSocketSink {
            close_called: false,
        });

        sink.send(ZCPacket::new_with_payload(b"packet"))
            .await
            .expect_err("send should fail");
        sink.close().await.expect_err("close should fail");
        assert!(sink.inner.close_called);
    }

    #[tokio::test]
    async fn packet_sink_without_padding_uses_direct_send_path() {
        let mut sink = WebSocketPacketSink::new(RecordingWebSocketSink::default());
        sink.send(ZCPacket::new_with_payload(b"direct packet"))
            .await
            .unwrap();

        assert!(sink.pending.is_empty());
        assert_eq!(sink.inner.messages.len(), 1);
        assert!(sink.inner.messages[0].is_binary());
    }

    #[tokio::test]
    async fn packet_sink_padding_skips_missed_intervals_after_idle() {
        let interval = Duration::from_secs(60);
        let mut sink =
            WebSocketPacketSink::with_padding(RecordingWebSocketSink::default(), interval, 64);
        sink.padding.as_mut().unwrap().next_at = std::time::Instant::now() - interval * 100;

        for _ in 0..3 {
            sink.send(ZCPacket::new_with_payload(b"packet"))
                .await
                .unwrap();
        }
        assert_eq!(sink.inner.messages.len(), 4);
        assert!(sink.inner.messages[0].is_text());
        assert!(sink.inner.messages[1..].iter().all(Message::is_binary));
        assert!(sink.padding.as_ref().unwrap().next_at > std::time::Instant::now());

        sink.padding.as_mut().unwrap().next_at = std::time::Instant::now() - interval;
        sink.send(ZCPacket::new_with_payload(b"next packet"))
            .await
            .unwrap();
        assert_eq!(sink.inner.messages.len(), 6);
        assert!(sink.inner.messages[4].is_text());
        assert!(sink.inner.messages[5].is_binary());
    }

    #[tokio::test]
    async fn map_from_ws_message_keeps_real_packet_with_zero_first_byte() {
        // A real ZCPacket over a ws/wss tunnel starts with its
        // `PeerManagerHeader`; the first byte is the low byte of
        // `from_peer_id` (little-endian) and can legitimately be 0x00 (e.g.
        // web-secure / foreign-network packets use from_peer_id = 0). Such a
        // binary frame must not be mistaken for padding.
        let packet_bytes = vec![0x00u8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
        let result = map_from_ws_message(Ok(Message::binary(packet_bytes.clone())), true)
            .await
            .expect("real packet must not be dropped");
        let packet = result.expect("real packet must decode");
        assert_eq!(packet.tunnel_payload_bytes().as_ref(), &packet_bytes[..]);
    }

    #[tokio::test]
    async fn map_from_ws_message_drops_text_padding_frame() {
        // Padding is carried as a WebSocket text frame; it must be silently
        // dropped without touching the (encrypted or not) payload.
        let result = map_from_ws_message(Ok(Message::text("cGFkZGluZw==".to_owned())), true).await;
        assert!(result.is_none(), "text padding frame must be dropped");
    }

    #[tokio::test]
    async fn map_from_ws_message_reports_unexpected_text_frame() {
        let result = map_from_ws_message(Ok(Message::text("cGFkZGluZw==".to_owned())), false).await;
        assert!(matches!(result, Some(Err(TunnelError::InvalidPacket(_)))));
    }
}
