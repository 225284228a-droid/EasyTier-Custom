//! Test-only peer-context fakes shared by peer-domain unit tests
//! (`peers::tests`, `peers::route::peer_ospf_route::tests`, and
//! `context::tests`). Kept out of `context.rs` so the context unit tests and
//! their consumers share one definition.

use std::net::IpAddr;

use cidr::{Ipv4Inet, Ipv6Inet};
use easytier_proto::common::{Flags, SecureModeConfig};
use hmac::Hmac;
use sha2::Sha256;

use crate::{
    config::peers::PeerRuntimeConfig,
    config::{CoreConfig, IpPrefix, NodeConfig, PeerPolicyConfig, RouteConfig, TrafficConfig},
    peers::context::{NetworkIdentity, PeerContext, secret_proof_from_secret},
};

pub(crate) trait PeerContextTestExt: PeerContext {
    fn runtime_config(&self) -> PeerRuntimeConfig {
        let network_identity = self.network_identity();
        let hostname = self.hostname();
        PeerRuntimeConfig {
            core: CoreConfig {
                node: NodeConfig {
                    peer_id: None,
                    instance_id: Some(*self.instance_id().as_bytes()),
                    hostname: (!hostname.is_empty()).then_some(hostname),
                    network_name: network_identity.network_name.clone(),
                },
                routes: RouteConfig {
                    ipv4: self.ipv4().map(ipv4_inet_to_config),
                    ipv6: self.ipv6().map(ipv6_inet_to_config),
                    ..Default::default()
                },
                peer_policy: PeerPolicyConfig::default(),
                traffic: TrafficConfig::default(),
            },
            network_identity,
            stun_info: self.stun_info(),
            feature_flags: self.feature_flags(),
            secure_mode: self.secure_mode(),
            host_routing: self.host_routing_policy(),
        }
    }
}

fn ipv4_inet_to_config(value: Ipv4Inet) -> IpPrefix {
    IpPrefix::new(IpAddr::V4(value.address()), value.network_length())
        .expect("Ipv4Inet should always have a valid IPv4 prefix length")
}

fn ipv6_inet_to_config(value: Ipv6Inet) -> IpPrefix {
    IpPrefix::new(IpAddr::V6(value.address()), value.network_length())
        .expect("Ipv6Inet should always have a valid IPv6 prefix length")
}

#[derive(Debug, Clone)]
pub(crate) struct NoopPeerContext {
    network_identity: NetworkIdentity,
    flags: Flags,
    secure_mode: Option<SecureModeConfig>,
}

impl NoopPeerContext {
    pub(crate) fn new(network_identity: NetworkIdentity) -> Self {
        Self {
            network_identity,
            flags: Flags::default(),
            secure_mode: None,
        }
    }

    pub(crate) fn with_secure_mode(mut self, secure_mode: SecureModeConfig) -> Self {
        self.flags.encryption_algorithm = "aes-gcm".to_owned();
        self.secure_mode = Some(secure_mode);
        self
    }

    pub(crate) fn with_flags(mut self, flags: Flags) -> Self {
        self.flags = flags;
        self
    }
}

impl Default for NoopPeerContext {
    fn default() -> Self {
        Self::new(NetworkIdentity::default())
    }
}

impl PeerContext for NoopPeerContext {
    fn network_identity(&self) -> NetworkIdentity {
        self.network_identity.clone()
    }

    fn flags(&self) -> Flags {
        self.flags.clone()
    }

    fn secure_mode(&self) -> Option<SecureModeConfig> {
        self.secure_mode.clone()
    }

    fn secret_proof(&self, challenge: &[u8]) -> Option<Hmac<Sha256>> {
        let secret = self.network_identity.network_secret.as_ref()?;
        secret_proof_from_secret(secret, challenge)
    }
}

impl PeerContextTestExt for NoopPeerContext {}

/// A bounded, lossless, in-order tunnel pair for tests: a send waits for
/// capacity, as a stream transport does, instead of dropping like the ring
/// tunnel. Shared by the peer and peer-manager test suites.
#[cfg(test)]
pub(crate) mod bounded_tunnel {
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::task::{Context as TaskContext, Poll};

    use futures::Sink;
    use tokio::sync::mpsc;

    use crate::packet::ZCPacket;
    use crate::proto::common::TunnelInfo;
    use crate::tunnel::{SplitTunnel, Tunnel, TunnelError};

    pub(crate) struct BoundedTunnel {
        rx: StdMutex<Option<mpsc::Receiver<ZCPacket>>>,
        tx: mpsc::Sender<ZCPacket>,
        info: Option<TunnelInfo>,
    }

    pub(crate) fn bounded_tunnel_pair(
        cap: usize,
        info: Option<TunnelInfo>,
    ) -> (BoundedTunnel, BoundedTunnel) {
        let (a_tx, b_rx) = mpsc::channel(cap);
        let (b_tx, a_rx) = mpsc::channel(cap);
        (
            BoundedTunnel {
                rx: StdMutex::new(Some(a_rx)),
                tx: a_tx,
                info: info.clone(),
            },
            BoundedTunnel {
                rx: StdMutex::new(Some(b_rx)),
                tx: b_tx,
                info,
            },
        )
    }

    struct BoundedSink {
        inner: tokio_util::sync::PollSender<ZCPacket>,
    }

    impl Sink<ZCPacket> for BoundedSink {
        type Error = TunnelError;
        fn poll_ready(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.inner)
                .poll_ready(cx)
                .map_err(|_| TunnelError::Shutdown)
        }
        fn start_send(mut self: Pin<&mut Self>, item: ZCPacket) -> Result<(), Self::Error> {
            Pin::new(&mut self.inner)
                .start_send(item)
                .map_err(|_| TunnelError::Shutdown)
        }
        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.inner)
                .poll_flush(cx)
                .map_err(|_| TunnelError::Shutdown)
        }
        fn poll_close(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.inner)
                .poll_close(cx)
                .map_err(|_| TunnelError::Shutdown)
        }
    }

    impl Tunnel for BoundedTunnel {
        fn split(&self) -> SplitTunnel {
            let rx = self.rx.lock().unwrap().take().unwrap();
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|packet| (Ok(packet), rx))
            });
            (
                Box::pin(stream),
                Box::pin(BoundedSink {
                    inner: tokio_util::sync::PollSender::new(self.tx.clone()),
                }),
            )
        }
        fn info(&self) -> Option<TunnelInfo> {
            self.info.clone()
        }
    }
}
