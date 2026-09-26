use std::{future::Future, net::SocketAddr, sync::Arc};

use futures::stream::FuturesUnordered;

use crate::socket::{
    IpVersion,
    udp::{
        UdpBindOptions, UdpSession, UdpSessionLayer, UdpSessionProtocol, VirtualUdpSocketFactory,
    },
};

use super::first_success;

pub struct ConnectedUdpSession {
    session: UdpSession,
    keep_alive: Box<dyn Send + Sync>,
}

impl ConnectedUdpSession {
    pub fn new<T>(session: UdpSession, keep_alive: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self {
            session,
            keep_alive: Box::new(keep_alive),
        }
    }

    pub fn session(&self) -> &UdpSession {
        &self.session
    }

    pub fn into_parts(self) -> (UdpSession, Box<dyn Send + Sync>) {
        (self.session, self.keep_alive)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpSessionMode {
    EasyTierMux,
    Classified(UdpSessionProtocol),
}

pub async fn connect_udp<H>(
    host: Arc<H>,
    remote_addr: SocketAddr,
    bind_addrs: Vec<SocketAddr>,
    default_bind: UdpBindOptions,
    mode: UdpSessionMode,
) -> anyhow::Result<ConnectedUdpSession>
where
    H: VirtualUdpSocketFactory,
{
    connect_udp_with(
        host,
        remote_addr,
        bind_addrs,
        default_bind,
        mode,
        |connected| async move { Ok(connected) },
    )
    .await
}

/// Races complete UDP connection attempts, including protocol-specific setup.
pub(crate) async fn connect_udp_with<H, C, F, T>(
    host: Arc<H>,
    remote_addr: SocketAddr,
    bind_addrs: Vec<SocketAddr>,
    default_bind: UdpBindOptions,
    mode: UdpSessionMode,
    completion: C,
) -> anyhow::Result<T>
where
    H: VirtualUdpSocketFactory,
    C: Fn(ConnectedUdpSession) -> F + Clone + Send,
    F: Future<Output = anyhow::Result<T>> + Send,
{
    let ip_version = if remote_addr.is_ipv4() {
        IpVersion::V4
    } else {
        IpVersion::V6
    };
    let default_bind = default_bind.with_ip_version(ip_version);
    let futures = FuturesUnordered::new();
    if bind_addrs.is_empty() {
        let local_addr = if remote_addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("static IPv4 bind address")
        } else {
            "[::]:0".parse().expect("static IPv6 bind address")
        };
        let bind = default_bind
            .with_local_addr(Some(local_addr))
            .with_only_v6(true);
        futures.push(bind_and_complete(
            host.clone(),
            bind,
            remote_addr,
            mode,
            completion.clone(),
        ));
    } else {
        for bind_addr in bind_addrs {
            let bind = default_bind
                .clone()
                .with_local_addr(Some(bind_addr))
                .with_only_v6(true);
            futures.push(bind_and_complete(
                host.clone(),
                bind,
                remote_addr,
                mode,
                completion.clone(),
            ));
        }
    }
    first_success(futures).await
}

async fn bind_and_complete<H, C, F, T>(
    host: Arc<H>,
    bind: UdpBindOptions,
    remote_addr: SocketAddr,
    mode: UdpSessionMode,
    completion: C,
) -> anyhow::Result<T>
where
    H: VirtualUdpSocketFactory,
    C: Fn(ConnectedUdpSession) -> F + Send,
    F: Future<Output = anyhow::Result<T>> + Send,
{
    let connected = bind_and_connect(host, bind, remote_addr, mode).await?;
    completion(connected).await
}

async fn bind_and_connect<H>(
    host: Arc<H>,
    bind: UdpBindOptions,
    remote_addr: SocketAddr,
    mode: UdpSessionMode,
) -> anyhow::Result<ConnectedUdpSession>
where
    H: VirtualUdpSocketFactory,
{
    let socket = host.bind_udp(bind).await?;
    let layer = Arc::new(UdpSessionLayer::new_with_stun_responder(socket, host));
    let session = match mode {
        UdpSessionMode::EasyTierMux => layer.connect(remote_addr).await?,
        UdpSessionMode::Classified(protocol) => {
            layer.open_classified_session(protocol, remote_addr)?
        }
    };
    Ok(ConnectedUdpSession::new(session, layer))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use tokio::sync::Notify;

    use crate::{host::testkit::TestHost, socket::udp::UdpSessionSocket};

    use super::*;

    fn candidate_addrs() -> Vec<SocketAddr> {
        vec![
            "192.0.2.1:0".parse().unwrap(),
            "192.0.2.2:0".parse().unwrap(),
        ]
    }

    fn classified_mode() -> UdpSessionMode {
        UdpSessionMode::Classified(UdpSessionProtocol::Quic)
    }

    #[tokio::test]
    async fn failed_protocol_completion_does_not_discard_viable_candidate() {
        let host = Arc::new(TestHost::default());
        let completed = Arc::new(AtomicUsize::new(0));
        let winner = connect_udp_with(
            host.clone(),
            "198.51.100.1:11014".parse().unwrap(),
            candidate_addrs(),
            UdpBindOptions::direct_connect(),
            classified_mode(),
            {
                let completed = completed.clone();
                move |connected| {
                    let completed = completed.clone();
                    async move {
                        completed.fetch_add(1, Ordering::Relaxed);
                        let local_addr = connected.session().local_addr()?;
                        if local_addr.ip() == candidate_addrs()[0].ip() {
                            anyhow::bail!("protocol upgrade failed");
                        }
                        Ok(connected)
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(
            winner.session().local_addr().unwrap().ip(),
            candidate_addrs()[1].ip()
        );
        assert_eq!(completed.load(Ordering::Relaxed), 2);
        assert_eq!(host.udp_binds.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn slow_protocol_completion_is_cancelled_when_another_candidate_finishes() {
        struct CompletionGuard(Arc<AtomicUsize>);

        impl Drop for CompletionGuard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let started = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let winner = tokio::time::timeout(
            Duration::from_secs(1),
            connect_udp_with(
                Arc::new(TestHost::default()),
                "198.51.100.1:11014".parse().unwrap(),
                candidate_addrs(),
                UdpBindOptions::direct_connect(),
                classified_mode(),
                {
                    let started = started.clone();
                    let dropped = dropped.clone();
                    move |connected| {
                        let started = started.clone();
                        let dropped = dropped.clone();
                        async move {
                            if connected.session().local_addr()?.ip() == candidate_addrs()[0].ip() {
                                let _guard = CompletionGuard(dropped);
                                started.notify_one();
                                std::future::pending::<()>().await;
                            } else {
                                started.notified().await;
                            }
                            Ok(connected)
                        }
                    }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            winner.session().local_addr().unwrap().ip(),
            candidate_addrs()[1].ip()
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn protocol_completion_errors_are_returned_after_all_candidates_fail() {
        let completed = Arc::new(AtomicUsize::new(0));
        let result = connect_udp_with(
            Arc::new(TestHost::default()),
            "198.51.100.1:11014".parse().unwrap(),
            candidate_addrs(),
            UdpBindOptions::direct_connect(),
            classified_mode(),
            {
                let completed = completed.clone();
                move |connected| {
                    let completed = completed.clone();
                    async move {
                        completed.fetch_add(1, Ordering::Relaxed);
                        let local_addr = connected.session().local_addr()?;
                        Err::<(), _>(anyhow::anyhow!("protocol upgrade failed for {local_addr}"))
                    }
                }
            },
        )
        .await;

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("protocol upgrade failed for")
        );
        assert_eq!(completed.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn connect_udp_preserves_classified_session_endpoint() {
        let host = Arc::new(TestHost::default());
        let remote_addr = "198.51.100.1:11014".parse().unwrap();
        let connected = connect_udp(
            host.clone(),
            remote_addr,
            Vec::new(),
            UdpBindOptions::direct_connect(),
            classified_mode(),
        )
        .await
        .unwrap();

        assert_eq!(connected.session().peer_addr().unwrap(), remote_addr);
        assert_eq!(
            connected.session().local_addr().unwrap(),
            "0.0.0.0:20002".parse().unwrap()
        );
        assert_eq!(host.udp_binds.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn easytier_mux_still_waits_for_the_transport_handshake() {
        let completed = Arc::new(AtomicUsize::new(0));
        let result = tokio::time::timeout(
            Duration::from_millis(10),
            connect_udp_with(
                Arc::new(TestHost::default()),
                "198.51.100.1:11010".parse().unwrap(),
                Vec::new(),
                UdpBindOptions::direct_connect(),
                UdpSessionMode::EasyTierMux,
                {
                    let completed = completed.clone();
                    move |connected| {
                        completed.fetch_add(1, Ordering::Relaxed);
                        async move { Ok(connected) }
                    }
                },
            ),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(completed.load(Ordering::Relaxed), 0);
    }
}
