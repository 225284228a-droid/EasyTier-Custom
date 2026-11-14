//! An in-memory datagram path for tunnel tests: one-way delay, in-order
//! delivery and seeded random loss on the forward direction.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use easytier_core::socket::udp::{UdpSessionKind, UdpSessionSocket};
use tokio::sync::mpsc;

pub(crate) const CLIENT: &str = "127.0.0.1:41001";
pub(crate) const SERVER: &str = "127.0.0.1:41002";

/// A datagram and when it reaches the other end.
type InFlight = (Instant, Vec<u8>);

pub(crate) struct DelayLink {
    /// Datagrams in flight, delivered in order once their time comes.
    tx: mpsc::UnboundedSender<InFlight>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    local: SocketAddr,
    peer: SocketAddr,
    delay: Duration,
    loss: f64,
    rng: Mutex<u64>,
}

#[async_trait]
impl UdpSessionSocket for DelayLink {
    fn kind(&self) -> UdpSessionKind {
        UdpSessionKind::Quic
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.local)
    }
    fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.peer)
    }
    async fn send(&self, data: &[u8]) -> std::io::Result<usize> {
        let lost = {
            let mut state = self.rng.lock().unwrap();
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            ((*state >> 11) as f64 / (1u64 << 53) as f64) < self.loss
        };
        if !lost {
            let _ = self.tx.send((Instant::now() + self.delay, data.to_vec()));
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

fn delay_line() -> (
    mpsc::UnboundedSender<InFlight>,
    mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<InFlight>();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some((at, datagram)) = in_rx.recv().await {
            tokio::time::sleep_until(at.into()).await;
            if out_tx.send(datagram).is_err() {
                break;
            }
        }
    });
    (in_tx, out_rx)
}

/// The client and server ends of a path that loses `loss` of the datagrams
/// the client sends and none of the server's.
pub(crate) fn pair(loss: f64, one_way: Duration) -> (Arc<DelayLink>, Arc<DelayLink>) {
    let (forward_tx, forward_rx) = delay_line();
    let (backward_tx, backward_rx) = delay_line();
    let client = Arc::new(DelayLink {
        tx: forward_tx,
        rx: tokio::sync::Mutex::new(backward_rx),
        local: CLIENT.parse().unwrap(),
        peer: SERVER.parse().unwrap(),
        delay: one_way,
        loss,
        rng: Mutex::new(0x9e37_79b9_7f4a_7c15),
    });
    let server = Arc::new(DelayLink {
        tx: backward_tx,
        rx: tokio::sync::Mutex::new(forward_rx),
        local: SERVER.parse().unwrap(),
        peer: CLIENT.parse().unwrap(),
        delay: one_way,
        loss: 0.0,
        rng: Mutex::new(1),
    });
    (client, server)
}
