use crate::socket_protector::ProtectedSocket;
use easytier_core::socket::tcp::TcpBindOptions;
use std::{io, net::SocketAddr};

#[derive(Debug)]
pub(super) enum NativeTcpListener {
    Single(ProtectedSocket<tokio::net::TcpListener>),
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    Physical(PhysicalListeners),
}

impl NativeTcpListener {
    pub(super) async fn bind(
        addr: SocketAddr,
        options: &TcpBindOptions,
    ) -> Result<Self, easytier_core::tunnel::TunnelError> {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if options.need_protect && addr.ip().is_unspecified() {
            return Ok(Self::Physical(
                PhysicalListeners::bind(addr, options.clone()).await?,
            ));
        }
        Ok(Self::Single(
            super::create_tcp_socket(addr, options)
                .await?
                .listen(1024)?,
        ))
    }

    pub(super) fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Single(listener) => listener.local_addr(),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            Self::Physical(listeners) => Ok(listeners.logical_addr),
        }
    }

    pub(super) async fn accept(&self) -> io::Result<(tokio::net::TcpStream, SocketAddr)> {
        match self {
            Self::Single(listener) => listener.accept().await,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            Self::Physical(listeners) => listeners.accept().await,
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Debug)]
struct PhysicalListenerState {
    listeners: tokio::sync::RwLock<
        std::collections::BTreeMap<
            SocketAddr,
            std::sync::Arc<ProtectedSocket<tokio::net::TcpListener>>,
        >,
    >,
    changed: tokio::sync::Notify,
}

/// A wildcard transport keeps a logical wildcard URL while each kernel
/// listener has a physical source address/interface before listen(). Thus
/// kernel SYNACKs, accepted children and existing sessions stay on that
/// interface even when a global route takeover is enabled later.
#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Debug)]
pub(super) struct PhysicalListeners {
    logical_addr: SocketAddr,
    state: std::sync::Arc<PhysicalListenerState>,
    _refresh: tokio_util::task::AbortOnDropHandle<()>,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl PhysicalListeners {
    async fn bind(
        mut addr: SocketAddr,
        options: TcpBindOptions,
    ) -> Result<Self, easytier_core::tunnel::TunnelError> {
        if addr.port() == 0 {
            // Reserve a logical port without ever starting a wildcard
            // listener. Its protection registration is dropped before the
            // per-interface sockets bind to this selected port.
            let reservation = super::create_tcp_socket(addr, &options).await?;
            addr.set_port(reservation.local_addr()?.port());
            drop(reservation);
        }
        let state = std::sync::Arc::new(PhysicalListenerState {
            listeners: tokio::sync::RwLock::new(std::collections::BTreeMap::new()),
            changed: tokio::sync::Notify::new(),
        });
        refresh(&state, addr, &options).await?;
        let weak = std::sync::Arc::downgrade(&state);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let Some(state) = weak.upgrade() else {
                    break;
                };
                if let Err(error) = refresh(&state, addr, &options).await {
                    tracing::warn!(?error, %addr, "could not refresh physical transport listeners");
                }
            }
        });
        Ok(Self {
            logical_addr: addr,
            state,
            _refresh: tokio_util::task::AbortOnDropHandle::new(task),
        })
    }

    async fn accept(&self) -> io::Result<(tokio::net::TcpStream, SocketAddr)> {
        use futures::StreamExt;
        loop {
            // Register the wakeup before taking the snapshot so a newly
            // attached interface cannot be missed between snapshot and await.
            let changed = self.state.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let listeners = self
                .state
                .listeners
                .read()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            if listeners.is_empty() {
                changed.await;
                continue;
            }
            let mut accepts = futures::stream::FuturesUnordered::new();
            for listener in listeners {
                accepts.push(async move { listener.accept().await });
            }
            tokio::select! {
                result = accepts.next() => if let Some(result) = result { return result; },
                _ = changed => {}
            }
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
async fn refresh(
    state: &PhysicalListenerState,
    logical: SocketAddr,
    options: &TcpBindOptions,
) -> Result<(), easytier_core::tunnel::TunnelError> {
    let net_ns = crate::common::netns::NetNS::from_socket_context(&options.context);
    let interfaces = crate::common::network::collect_interfaces(
        net_ns,
        options
            .bind_device
            .as_ref()
            .is_none_or(|device| device.is_empty()),
    )
    .await;
    let mut desired = std::collections::BTreeSet::new();
    let allow_v4 = logical.is_ipv4() || !options.only_v6;
    let allow_v6 = logical.is_ipv6();
    for interface in interfaces {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::NetworkManagement::IpHelper::{GetIfEntry2, MIB_IF_ROW2};
            use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
            let mut row = MIB_IF_ROW2 {
                InterfaceIndex: interface.index,
                ..Default::default()
            };
            let result = unsafe { GetIfEntry2(&mut row) };
            if result.0 != 0 || row.OperStatus != IfOperStatusUp {
                continue;
            }
        }
        if let Some(device) = options
            .bind_device
            .as_ref()
            .filter(|device| !device.is_empty())
            && device != &interface.name
        {
            continue;
        }
        for address in interface.addr {
            let ip = address.ip();
            if ip.is_unspecified() || ip.is_multicast() {
                continue;
            }
            if ip.is_ipv4() && allow_v4 {
                desired.insert(SocketAddr::new(ip, logical.port()));
            }
            if let std::net::IpAddr::V6(ip) = ip
                && allow_v6
            {
                let scope = if ip.is_unicast_link_local() {
                    interface.index
                } else {
                    0
                };
                desired.insert(SocketAddr::V6(std::net::SocketAddrV6::new(
                    ip,
                    logical.port(),
                    0,
                    scope,
                )));
            }
        }
    }
    // Preserve local reachability while offline, and avoid starting any
    // kernel wildcard listener that a future VPN takeover could capture.
    if options
        .bind_device
        .as_ref()
        .is_none_or(|device| device.is_empty())
    {
        if allow_v4 {
            desired.insert(SocketAddr::from(([127, 0, 0, 1], logical.port())));
        }
        if allow_v6 {
            desired.insert(SocketAddr::from((
                std::net::Ipv6Addr::LOCALHOST,
                logical.port(),
            )));
        }
    }
    let mut listeners = state.listeners.write().await;
    let mut added = std::collections::BTreeMap::new();
    // Prepare all new listeners before changing the active collection. A
    // bind failure preserves every listener and accepted connection already
    // in service; the periodic updater retries after interface changes.
    for addr in desired
        .difference(&listeners.keys().copied().collect())
        .copied()
    {
        let mut bind = options.clone();
        bind.local_addr = Some(addr);
        if bind
            .bind_device
            .as_ref()
            .is_none_or(|device| device.is_empty())
        {
            bind.bind_device = crate::tunnel::common::get_interface_name_by_ip(&addr.ip());
        }
        let listener = super::create_tcp_socket(addr, &bind).await?.listen(1024)?;
        added.insert(addr, std::sync::Arc::new(listener));
    }
    let changed = !added.is_empty() || listeners.keys().any(|addr| !desired.contains(addr));
    listeners.extend(added);
    listeners.retain(|addr, _| desired.contains(addr));
    drop(listeners);
    if changed {
        state.changed.notify_waiters();
    }
    Ok(())
}

#[cfg(all(test, any(target_os = "windows", target_os = "macos")))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn protected_wildcard_keeps_logical_port_and_accepts_local_connections() {
        let address: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let listener = NativeTcpListener::bind(
            address,
            &TcpBindOptions::default().with_local_addr(Some(address)),
        )
        .await
        .unwrap();
        let logical = listener.local_addr().unwrap();
        assert!(logical.ip().is_unspecified());
        assert_ne!(logical.port(), 0);
        let NativeTcpListener::Physical(physical) = &listener else {
            panic!("protected wildcard must use physical listeners");
        };
        let bound = physical.state.listeners.read().await;
        assert!(
            bound
                .keys()
                .all(|address| !address.ip().is_unspecified() && address.port() == logical.port())
        );
        assert!(bound.contains_key(&SocketAddr::from(([127, 0, 0, 1], logical.port()))));
        drop(bound);
        let client =
            tokio::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], logical.port())))
                .await
                .unwrap();
        let (server, _) =
            tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            server.local_addr().unwrap().ip(),
            std::net::Ipv4Addr::LOCALHOST
        );
        drop(client);
    }
}
