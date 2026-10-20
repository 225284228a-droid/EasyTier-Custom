use std::{pin::Pin, time::Duration};

use futures::SinkExt;
use tokio::{
    sync::mpsc::{Receiver, Sender, channel, error::TrySendError},
    task::JoinHandle,
};

use crate::{
    foundation::time::timeout,
    packet::ZCPacket,
    proto::common::TunnelInfo,
    tunnel::{Tunnel, TunnelError, ZCPacketSink, ZCPacketStream},
};

#[derive(Clone)]
pub struct MpscTunnelSender(Sender<ZCPacket>);

impl MpscTunnelSender {
    #[cfg(test)]
    pub(crate) fn from_sender(sender: Sender<ZCPacket>) -> Self {
        Self(sender)
    }

    pub async fn reserve_owned(
        &self,
    ) -> Result<tokio::sync::mpsc::OwnedPermit<ZCPacket>, TunnelError> {
        self.0
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| TunnelError::Shutdown)
    }
    /// Keep ownership on congestion so a striped packet can try another path.
    pub fn try_send_owned(&self, item: ZCPacket) -> Result<(), TrySendError<ZCPacket>> {
        self.0.try_send(item)
    }
    pub async fn send(&self, item: ZCPacket) -> Result<(), TunnelError> {
        match self.0.try_send(item) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(item)) => {
                self.0.send(item).await.map_err(|_| TunnelError::Shutdown)
            }
            Err(TrySendError::Closed(_)) => Err(TunnelError::Shutdown),
        }
    }

    pub fn try_send(&self, item: ZCPacket) -> Result<(), TunnelError> {
        self.0.try_send(item).map_err(|e| match e {
            TrySendError::Full(_) => TunnelError::BufferFull,
            TrySendError::Closed(_) => TunnelError::Shutdown,
        })
    }
}

pub struct MpscTunnel<T> {
    tx: Option<Sender<ZCPacket>>,
    tunnel: T,
    stream: Option<Pin<Box<dyn ZCPacketStream>>>,
    task: JoinHandle<()>,
}

impl<T: Tunnel> MpscTunnel<T> {
    pub fn new(tunnel: T, send_timeout: Option<Duration>) -> Self {
        let (tx, mut rx) = channel(32);
        let (stream, mut sink) = tunnel.split();

        let task = tokio::spawn(async move {
            loop {
                if let Err(e) = Self::forward_one_round(&mut rx, &mut sink, send_timeout).await {
                    tracing::error!(?e, "forward error");
                    break;
                }
            }
            rx.close();
            let close_ret = timeout(Duration::from_secs(5), sink.close()).await;
            tracing::warn!(?close_ret, "mpsc close sink");
        });

        Self {
            tx: Some(tx),
            tunnel,
            stream: Some(stream),
            task,
        }
    }

    async fn forward_one_round(
        rx: &mut Receiver<ZCPacket>,
        sink: &mut Pin<Box<dyn ZCPacketSink>>,
        send_timeout_ms: Option<Duration>,
    ) -> Result<(), TunnelError> {
        let item = rx.recv().await.ok_or(TunnelError::Shutdown)?;
        if let Some(timeout_ms) = send_timeout_ms {
            Self::forward_one_round_with_timeout(rx, sink, item, timeout_ms).await
        } else {
            Self::forward_one_round_no_timeout(rx, sink, item).await
        }
    }

    async fn forward_one_round_no_timeout(
        rx: &mut Receiver<ZCPacket>,
        sink: &mut Pin<Box<dyn ZCPacketSink>>,
        initial_item: ZCPacket,
    ) -> Result<(), TunnelError> {
        match sink.feed(initial_item).await {
            Ok(()) | Err(TunnelError::BufferFull) => {}
            Err(error) => return Err(error),
        }

        while let Ok(item) = rx.try_recv() {
            if let Err(e) = sink.feed(item).await {
                tracing::error!(?e, "feed error");
                if !matches!(e, TunnelError::BufferFull) {
                    return Err(e);
                }
            }
        }

        sink.flush().await
    }

    async fn forward_one_round_with_timeout(
        rx: &mut Receiver<ZCPacket>,
        sink: &mut Pin<Box<dyn ZCPacketSink>>,
        initial_item: ZCPacket,
        timeout_ms: Duration,
    ) -> Result<(), TunnelError> {
        match timeout(timeout_ms, async move {
            Self::forward_one_round_no_timeout(rx, sink, initial_item).await
        })
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                tracing::error!(?e, "forward error");
                Err(e)
            }
            Err(e) => {
                tracing::error!(?e, "forward timeout");
                Err(e.into())
            }
        }
    }

    pub fn get_stream(&mut self) -> Pin<Box<dyn ZCPacketStream>> {
        self.stream.take().unwrap()
    }

    pub fn get_sink(&self) -> MpscTunnelSender {
        MpscTunnelSender(self.tx.as_ref().unwrap().clone())
    }

    pub fn close(&mut self) {
        self.tx.take();
        self.task.abort();
    }

    pub fn tunnel_info(&self) -> Option<TunnelInfo> {
        self.tunnel.info()
    }
}

impl<T> Drop for MpscTunnel<T> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt, stream::FuturesUnordered};

    #[test]
    fn try_send_owned_returns_full_packet_for_another_channel() {
        let (full_tx, _full_rx) = channel(1);
        let full_sender = MpscTunnelSender(full_tx);
        full_sender
            .try_send_owned(ZCPacket::new_with_payload(b"occupy"))
            .unwrap();

        let returned = match full_sender.try_send_owned(ZCPacket::new_with_payload(b"retry")) {
            Err(TrySendError::Full(packet)) => packet,
            Err(TrySendError::Closed(_)) => panic!("channel unexpectedly closed"),
            Ok(()) => panic!("full channel unexpectedly accepted packet"),
        };
        assert_eq!(returned.payload(), b"retry");

        let (retry_tx, mut retry_rx) = channel(1);
        MpscTunnelSender(retry_tx).try_send_owned(returned).unwrap();
        assert_eq!(retry_rx.try_recv().unwrap().payload(), b"retry");
    }

    #[test]
    fn try_send_owned_returns_packet_when_channel_is_closed() {
        let (tx, rx) = channel(1);
        drop(rx);
        let sender = MpscTunnelSender(tx);

        let returned = match sender.try_send_owned(ZCPacket::new_with_payload(b"closed")) {
            Err(TrySendError::Closed(packet)) => packet,
            Err(TrySendError::Full(_)) => panic!("closed channel reported full"),
            Ok(()) => panic!("closed channel unexpectedly accepted packet"),
        };
        assert_eq!(returned.payload(), b"closed");
    }

    #[tokio::test]
    async fn any_ready_owned_permit_does_not_wait_for_stalled_first_member() {
        type ReserveFuture = Pin<
            Box<
                dyn std::future::Future<
                        Output = (
                            usize,
                            Result<
                                tokio::sync::mpsc::OwnedPermit<ZCPacket>,
                                tokio::sync::mpsc::error::SendError<()>,
                            >,
                        ),
                    > + Send,
            >,
        >;

        let (first_tx, mut first_rx) = channel(1);
        let (second_tx, second_rx) = channel(1);
        first_tx
            .send(ZCPacket::new_with_payload(b"first-original"))
            .await
            .unwrap();
        second_tx
            .send(ZCPacket::new_with_payload(b"second-original"))
            .await
            .unwrap();

        let release_second = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut second_rx = second_rx;
            let original = second_rx.recv().await.unwrap();
            (original, second_rx)
        });

        let mut reserves: FuturesUnordered<ReserveFuture> = FuturesUnordered::new();
        reserves.push(Box::pin(async move { (0, first_tx.reserve_owned().await) }));
        reserves.push(Box::pin(
            async move { (1, second_tx.reserve_owned().await) },
        ));

        let (winner, permit) = tokio::time::timeout(Duration::from_secs(1), async {
            match reserves.next().await {
                Some((index, Ok(permit))) => (index, permit),
                Some((_, Err(error))) => panic!("reserve unexpectedly failed: {error}"),
                None => panic!("all reserve futures ended without a permit"),
            }
        })
        .await
        .expect("a permit should become ready after releasing the second channel");
        assert_eq!(winner, 1);

        let (released_packet, mut second_rx) = release_second.await.unwrap();
        assert_eq!(released_packet.payload(), b"second-original");
        permit.send(ZCPacket::new_with_payload(b"second-retry"));

        assert_eq!(first_rx.try_recv().unwrap().payload(), b"first-original");
        assert_eq!(second_rx.try_recv().unwrap().payload(), b"second-retry");
    }

    #[tokio::test]
    async fn sender_falls_back_to_waiting_when_channel_is_full() {
        let (tx, mut rx) = channel(1);
        let sender = MpscTunnelSender(tx);
        sender
            .send(ZCPacket::new_with_payload(b"first"))
            .await
            .unwrap();

        let blocked_send = sender.send(ZCPacket::new_with_payload(b"second"));
        tokio::pin!(blocked_send);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut blocked_send)
                .await
                .is_err()
        );

        assert_eq!(rx.recv().await.unwrap().payload(), b"first");
        tokio::time::timeout(Duration::from_secs(1), &mut blocked_send)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rx.recv().await.unwrap().payload(), b"second");
    }
}
