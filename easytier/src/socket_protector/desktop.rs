//! Process-owned native socket registrations and desktop routing bypass.
//!
//! A registration owns a duplicate handle, never a borrowed raw handle. The
//! registry holds only weak references, so fd reuse cannot protect an unrelated
//! socket and TCP close is delayed only until its last reader/writer is dropped.

use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex, OnceLock, Weak},
};

#[derive(Debug)]
pub(crate) struct SocketRegistration {
    socket: socket2::Socket,
    namespace: u64,
    io_lock: Mutex<()>,
}

#[derive(Clone)]
struct Protection {
    bit: u32,
    defaults: [Option<u32>; 2],
    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    interface_routes: Arc<[(easytier_core::config::vpn::VpnCidr, u32)]>,
    owners: usize,
}

#[derive(Default)]
struct Registry {
    sockets: Vec<Weak<SocketRegistration>>,
    active: BTreeMap<u64, Protection>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

pub(crate) fn available() -> bool {
    // Resolve bootstrap DNS through the protected factory even before the
    // first route takeover, so pooled sockets can participate in hot takeover.
    cfg!(any(
        target_os = "linux",
        target_os = "windows",
        target_os = "macos"
    ))
}

pub(crate) fn active() -> bool {
    !registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active
        .is_empty()
}

#[cfg(target_os = "linux")]
fn namespace(socket: &socket2::Socket) -> io::Result<u64> {
    use std::os::fd::AsRawFd;
    let mut cookie = 0_u64;
    let mut len = std::mem::size_of_val(&cookie) as libc::socklen_t;
    // SO_NETNS_COOKIE is stable since Linux 5.2 and identifies the socket's
    // namespace independently of the calling Tokio worker's current namespace.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            71,
            (&mut cookie as *mut u64).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cookie)
}

#[cfg(not(target_os = "linux"))]
fn namespace(_socket: &socket2::Socket) -> io::Result<u64> {
    Ok(0)
}

pub(crate) fn namespace_for(net_ns: &crate::common::netns::NetNS) -> io::Result<u64> {
    net_ns.run(|| {
        namespace(&socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?)
    })
}

pub(crate) fn register(socket: &socket2::Socket) -> io::Result<Option<Arc<SocketRegistration>>> {
    if !available() {
        return Ok(None);
    }
    let entry = Arc::new(SocketRegistration {
        socket: socket.try_clone()?,
        namespace: namespace(socket)?,
        io_lock: Mutex::new(()),
    });
    let mut state = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(protection) = state.active.get(&entry.namespace) {
        #[cfg(target_os = "linux")]
        if socket.mark()? & protection.bit != 0
            && socket.peer_addr().is_err()
            && socket
                .local_addr()?
                .as_socket()
                .is_some_and(|address| address.port() == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "configured socket mark conflicts with the active VPN bypass bit",
            ));
        }
        apply(&entry.socket, protection)?;
    }
    state.sockets.retain(|entry| entry.strong_count() != 0);
    state.sockets.push(Arc::downgrade(&entry));
    Ok(Some(entry))
}

pub(crate) fn refresh_scope(socket: &socket2::Socket) -> io::Result<()> {
    if !available() {
        return Ok(());
    }
    let state = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(protection) = state.active.get(&namespace(socket)?) {
        apply(socket, protection)?;
    }
    Ok(())
}

pub(crate) fn prepare_tcp_connect(
    socket: &socket2::Socket,
    destination: std::net::SocketAddr,
) -> io::Result<()> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let state = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(protection) = state.active.get(&namespace(socket)?) {
            apply_scope(socket, protection, Some(destination))?;
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = (socket, destination);
    Ok(())
}

/// Scope only the synchronous datagram operation. A wildcard socket retains
/// wildcard reception and source metadata can select any physical interface.
pub(crate) fn with_udp_scope<T>(
    registration: Option<&Arc<SocketRegistration>>,
    socket: &socket2::Socket,
    source: Option<std::net::IpAddr>,
    destination: std::net::SocketAddr,
    operation: impl FnOnce() -> T,
) -> io::Result<T> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let Some(registration) = registration else {
            return Ok(operation());
        };
        let _io = registration
            .io_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let protection = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .get(&registration.namespace)
            .cloned();
        let Some(protection) = protection else {
            return Ok(operation());
        };
        let ipv6 = destination.ip().to_canonical().is_ipv6();
        let previous = get_scope(socket, ipv6)?;
        let source_interface = source
            .and_then(|source| {
                crate::tunnel::common::get_interface_name_by_ip(&source.to_canonical())
            })
            .and_then(|name| interface_index(&name));
        let local_interface = socket
            .local_addr()?
            .as_socket()
            .and_then(|addr| {
                crate::tunnel::common::get_interface_name_by_ip(&addr.ip().to_canonical())
            })
            .and_then(|name| interface_index(&name));
        let index = source_interface
            .or((previous != 0).then_some(previous))
            .or(local_interface)
            .or_else(|| destination_interface(&protection, destination))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NetworkUnreachable,
                    "VPN datagram bypass has no physical interface",
                )
            })?;
        set_scope(socket, ipv6, index)?;
        let result = operation();
        set_scope(socket, ipv6, previous)?;
        return Ok(result);
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (registration, socket, source, destination);
        Ok(operation())
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn used_socket_marks(namespace: u64) -> io::Result<u32> {
    let mut state = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.sockets.retain(|entry| entry.strong_count() != 0);
    let mut used = 0;
    #[cfg(target_os = "linux")]
    for socket in state
        .sockets
        .iter()
        .filter_map(Weak::upgrade)
        .filter(|entry| entry.namespace == namespace)
    {
        used |= socket.socket.mark()?;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = namespace;
    if let Some(active) = state.active.get(&namespace) {
        used &= !active.bit;
    }
    Ok(used)
}

#[cfg(target_os = "linux")]
pub(crate) fn active_bit(namespace: u64) -> Option<u32> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active
        .get(&namespace)
        .map(|active| active.bit)
}

/// Returns a bit outside every external rule's mask and every configured or
/// active socket mark. Exact-mask external rules consequently reject takeover.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn choose_bypass_bit(
    used: u32,
    external_masks: impl IntoIterator<Item = u32>,
) -> io::Result<u32> {
    let occupied = external_masks
        .into_iter()
        .fold(used, |all, mask| all | mask);
    (0..32)
        .rev()
        .map(|shift| 1_u32 << shift)
        .find(|bit| occupied & bit == 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "no unused VPN socket mark bit; existing fwmark rules or socket marks conflict",
            )
        })
}

pub(crate) struct DesktopProtectionLease {
    namespace: u64,
    bit: u32,
    released: bool,
}

impl DesktopProtectionLease {
    pub(crate) fn acquire(
        namespace: u64,
        bit: u32,
        defaults: [Option<u32>; 2],
    ) -> io::Result<Self> {
        let mut state = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = state.active.get_mut(&namespace) {
            if active.bit != bit {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another VPN owns this namespace's socket bypass",
                ));
            }
            active.owners += 1;
            return Ok(Self {
                namespace,
                bit,
                released: false,
            });
        }
        let protection = Protection {
            bit,
            defaults,
            #[cfg(any(target_os = "windows", target_os = "macos", test))]
            interface_routes: Arc::new([]),
            owners: 1,
        };
        let sockets = state
            .sockets
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|entry| entry.namespace == namespace)
            .collect::<Vec<_>>();
        let mut completed: Vec<Arc<SocketRegistration>> = Vec::new();
        for socket in sockets {
            if let Err(error) = apply(&socket.socket, &protection) {
                for socket in completed {
                    let _: io::Result<()> = clear(&socket.socket, bit);
                }
                return Err(error);
            }
            completed.push(socket);
        }
        // Registration and activation share this lock: every existing socket
        // is protected here, and every new one observes active before first IO.
        state.active.insert(namespace, protection);
        Ok(Self {
            namespace,
            bit,
            released: false,
        })
    }

    pub(crate) fn refresh_defaults(&mut self, defaults: [Option<u32>; 2]) -> io::Result<()> {
        let mut state = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = state.active.get_mut(&self.namespace) else {
            return Ok(());
        };
        active.defaults = defaults;
        let protection = active.clone();
        for socket in state
            .sockets
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|entry| entry.namespace == self.namespace)
        {
            apply(&socket.socket, &protection)?;
        }
        Ok(())
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub(crate) fn refresh_interfaces(
        &mut self,
        interfaces: Vec<network_interface::NetworkInterface>,
    ) {
        let routes = interfaces
            .into_iter()
            .filter(|interface| {
                #[cfg(target_os = "windows")]
                {
                    use windows::Win32::NetworkManagement::IpHelper::{GetIfEntry2, MIB_IF_ROW2};
                    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
                    let mut row = MIB_IF_ROW2 {
                        InterfaceIndex: interface.index,
                        ..Default::default()
                    };
                    unsafe { GetIfEntry2(&mut row) }.0 == 0 && row.OperStatus == IfOperStatusUp
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = interface;
                    true
                }
            })
            .flat_map(|interface| {
                interface.addr.into_iter().filter_map(move |address| {
                    let ip = address.ip();
                    let mask = match address {
                        network_interface::Addr::V4(address) => {
                            address.netmask.map(std::net::IpAddr::V4)
                        }
                        network_interface::Addr::V6(address) => {
                            address.netmask.map(std::net::IpAddr::V6)
                        }
                    }?;
                    let prefix = crate::common::network::ip_mask_to_prefix(mask).ok()?;
                    if prefix == 0 {
                        return None;
                    }
                    let network = match (ip, mask) {
                        (std::net::IpAddr::V4(ip), std::net::IpAddr::V4(mask)) => {
                            std::net::IpAddr::V4(std::net::Ipv4Addr::from(
                                u32::from(ip) & u32::from(mask),
                            ))
                        }
                        (std::net::IpAddr::V6(ip), std::net::IpAddr::V6(mask)) => {
                            std::net::IpAddr::V6(std::net::Ipv6Addr::from(
                                u128::from(ip) & u128::from(mask),
                            ))
                        }
                        _ => return None,
                    };
                    let cidr = format!("{network}/{prefix}").parse().ok()?;
                    Some((cidr, interface.index))
                })
            })
            .collect::<Vec<_>>();
        if let Some(active) = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .get_mut(&self.namespace)
        {
            active.interface_routes = routes.into();
        }
    }

    pub(crate) fn release(&mut self) -> io::Result<()> {
        if self.released {
            return Ok(());
        }
        let mut state = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = state.active.get_mut(&self.namespace) else {
            self.released = true;
            return Ok(());
        };
        if active.owners > 1 {
            active.owners -= 1;
            self.released = true;
            return Ok(());
        }
        // Called only after routes/policy rules are removed. Keep the owner
        // active if restoration fails so a later shutdown can retry safely.
        for socket in state
            .sockets
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|entry| entry.namespace == self.namespace)
        {
            clear(&socket.socket, self.bit)?;
        }
        state.active.remove(&self.namespace);
        self.released = true;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn apply(socket: &socket2::Socket, protection: &Protection) -> io::Result<()> {
    let mark = socket.mark()?;
    // Never erase configured marks or marks set by an external protector.
    socket.set_mark(mark | protection.bit)
}
#[cfg(target_os = "linux")]
fn clear(socket: &socket2::Socket, bit: u32) -> io::Result<()> {
    socket.set_mark(socket.mark()? & !bit)
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn apply(socket: &socket2::Socket, protection: &Protection) -> io::Result<()> {
    apply_scope(
        socket,
        protection,
        socket.peer_addr().ok().and_then(|addr| addr.as_socket()),
    )
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn destination_interface(
    protection: &Protection,
    destination: std::net::SocketAddr,
) -> Option<u32> {
    if let std::net::SocketAddr::V6(address) = destination
        && address.scope_id() != 0
    {
        return Some(address.scope_id());
    }
    let destination = destination.ip().to_canonical();
    protection
        .interface_routes
        .iter()
        .filter(|(cidr, _)| cidr.contains(&destination))
        .max_by_key(|(cidr, _)| cidr.prefix_len())
        .map(|(_, index)| *index)
        .or(protection.defaults[usize::from(destination.is_ipv6())])
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn apply_scope(
    socket: &socket2::Socket,
    protection: &Protection,
    destination: Option<std::net::SocketAddr>,
) -> io::Result<()> {
    // Datagram protection applies around send/connect after bind. Windows
    // getsockname rejects an unbound socket, so registration must not call it.
    if socket.r#type()? == socket2::Type::DGRAM {
        return Ok(());
    }
    let addr = match socket.local_addr() {
        Ok(address) => address
            .as_socket()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not an IP socket"))?,
        #[cfg(target_os = "windows")]
        Err(error) if error.raw_os_error() == Some(10022) => {
            let Some(destination) = destination else {
                return Ok(());
            };
            std::net::SocketAddr::new(
                if destination.is_ipv6() {
                    std::net::Ipv6Addr::UNSPECIFIED.into()
                } else {
                    std::net::Ipv4Addr::UNSPECIFIED.into()
                },
                0,
            )
        }
        Err(error) => return Err(error),
    };
    // A concrete local address identifies the requested physical interface.
    // A wildcard TCP listener must retain its ability to accept on every
    // interface; each accepted socket is re-protected before it is exposed.
    if addr.ip().is_unspecified() && destination.is_none() {
        return Ok(());
    }
    let ipv6 = destination
        .map_or(addr.ip(), |addr| addr.ip())
        .to_canonical()
        .is_ipv6();
    let previous = get_scope(socket, ipv6)?;
    let explicit = crate::tunnel::common::get_interface_name_by_ip(&addr.ip().to_canonical())
        .and_then(|name| interface_index(&name));
    let index = (previous != 0)
        .then_some(previous)
        .or(explicit)
        .or_else(|| {
            destination.and_then(|destination| destination_interface(protection, destination))
        })
        .or(protection.defaults[usize::from(ipv6)])
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NetworkUnreachable,
                "VPN bypass has no physical interface for socket family",
            )
        })?;
    set_scope(socket, ipv6, index)
}

#[cfg(target_os = "windows")]
fn get_scope(socket: &socket2::Socket, ipv6: bool) -> io::Result<u32> {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{
        IP_UNICAST_IF, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF, SOCKET, SOCKET_ERROR,
        WSAGetLastError, getsockopt,
    };
    let mut value = 0_u32;
    let mut length = std::mem::size_of_val(&value) as i32;
    let result = unsafe {
        getsockopt(
            SOCKET(socket.as_raw_socket() as usize),
            if ipv6 { IPPROTO_IPV6.0 } else { IPPROTO_IP.0 },
            if ipv6 { IPV6_UNICAST_IF } else { IP_UNICAST_IF },
            windows::core::PSTR((&mut value as *mut u32).cast::<u8>()),
            &mut length,
        )
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError().0 }));
    }
    // IP_UNICAST_IF sets use network byte order, but gets return host byte
    // order. IPV6_UNICAST_IF uses host byte order for both operations.
    Ok(value)
}

#[cfg(target_os = "macos")]
fn get_scope(socket: &socket2::Socket, ipv6: bool) -> io::Result<u32> {
    Ok(if ipv6 {
        socket.device_index_v6()?
    } else {
        socket.device_index_v4()?
    }
    .map_or(0, std::num::NonZeroU32::get))
}

#[cfg(target_os = "windows")]
fn set_scope(socket: &socket2::Socket, ipv6: bool, index: u32) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{
        IP_UNICAST_IF, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF, SOCKET, SOCKET_ERROR,
        WSAGetLastError, setsockopt,
    };
    let value = if ipv6 { index } else { index.to_be() }.to_ne_bytes();
    let result = unsafe {
        setsockopt(
            SOCKET(socket.as_raw_socket() as usize),
            if ipv6 { IPPROTO_IPV6.0 } else { IPPROTO_IP.0 },
            if ipv6 { IPV6_UNICAST_IF } else { IP_UNICAST_IF },
            Some(&value),
        )
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError().0 }));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_scope(socket: &socket2::Socket, ipv6: bool, index: u32) -> io::Result<()> {
    let index = std::num::NonZeroU32::new(index);
    if ipv6 {
        socket.bind_device_by_index_v6(index)
    } else {
        socket.bind_device_by_index_v4(index)
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn clear(_socket: &socket2::Socket, _bit: u32) -> io::Result<()> {
    // Keep each socket's selected physical scope for its remaining lifetime;
    // existing explicit source/interface constraints must never be reset.
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn interface_index(name: &str) -> Option<u32> {
    if let Some(index) = name.strip_prefix("if").and_then(|index| index.parse().ok()) {
        return Some(index);
    }
    crate::arch::windows::find_interface_index(name).ok()
}
#[cfg(target_os = "macos")]
pub(crate) fn interface_index(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    (index != 0).then_some(index)
}
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn interface_index(_name: &str) -> Option<u32> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn apply(_socket: &socket2::Socket, _protection: &Protection) -> io::Result<()> {
    Ok(())
}
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn clear(_socket: &socket2::Socket, _bit: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn native_scope_roundtrip_preserves_interface_index_and_restoration() {
        let interface =
            crate::tunnel::common::get_interface_name_by_ip(&std::net::Ipv4Addr::LOCALHOST.into())
                .and_then(|name| interface_index(&name))
                .unwrap();
        for ipv6 in [false, true] {
            let socket = socket2::Socket::new(
                if ipv6 {
                    socket2::Domain::IPV6
                } else {
                    socket2::Domain::IPV4
                },
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )
            .unwrap();
            let previous = get_scope(&socket, ipv6).unwrap();
            set_scope(&socket, ipv6, interface).unwrap();
            assert_eq!(get_scope(&socket, ipv6).unwrap(), interface);
            set_scope(&socket, ipv6, previous).unwrap();
            assert_eq!(get_scope(&socket, ipv6).unwrap(), previous);
        }
    }

    #[test]
    fn destination_scope_preserves_connected_private_loopback_and_mapped_routes() {
        let protection = Protection {
            bit: 0,
            defaults: [Some(1), Some(2)],
            interface_routes: [
                ("127.0.0.0/8", 3),
                ("::1/128", 3),
                ("192.168.1.0/24", 4),
                ("192.168.1.128/25", 5),
                ("fd12::/64", 6),
            ]
            .into_iter()
            .map(|(cidr, index)| (cidr.parse().unwrap(), index))
            .collect::<Vec<_>>()
            .into(),
            owners: 1,
        };
        for (destination, expected) in [
            ("127.0.0.53:53", 3),
            ("[::1]:53", 3),
            ("[::ffff:127.0.0.53]:53", 3),
            ("192.168.1.1:53", 4),
            ("192.168.1.200:53", 5),
            ("[fd12::53]:53", 6),
            ("203.0.113.1:53", 1),
            ("[::ffff:203.0.113.1]:53", 1),
            ("[2001:db8::1]:53", 2),
            ("[fe80::1%7]:53", 7),
        ] {
            assert_eq!(
                destination_interface(&protection, destination.parse().unwrap()),
                Some(expected),
                "{destination}"
            );
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[tokio::test]
    async fn active_protection_keeps_native_dns_and_tcp_reconnect_on_local_interfaces() {
        use network_interface::NetworkInterfaceConfig;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        struct Cleanup(DesktopProtectionLease);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.release().unwrap();
            }
        }
        let interfaces = network_interface::NetworkInterface::show().unwrap();
        let mut targets = vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)];
        if let Some(ip) = interfaces
            .iter()
            .filter(|interface| !interface.internal)
            .flat_map(|interface| &interface.addr)
            .map(|address| address.ip())
            .find(|ip| matches!(ip, std::net::IpAddr::V4(ip) if ip.is_private()))
        {
            targets.push(ip);
        }
        // An invalid fallback makes this test prove connected/loopback lookup
        // was used, rather than accidentally succeeding on a default route.
        let mut capture = Cleanup(
            DesktopProtectionLease::acquire(0, 0, [Some(u32::MAX), Some(u32::MAX)]).unwrap(),
        );
        capture.0.refresh_interfaces(interfaces);
        for ip in targets {
            let server = tokio::net::UdpSocket::bind(std::net::SocketAddr::new(ip, 0))
                .await
                .unwrap();
            let peer = server.local_addr().unwrap();
            let client = crate::socket::udp::create_udp_socket(
                &easytier_core::socket::udp::UdpBindOptions::direct_connect()
                    .with_local_addr(Some("0.0.0.0:0".parse().unwrap())),
            )
            .await
            .unwrap();
            hickory_proto::udp::DnsUdpSocket::send_to(&client, b"DNS query", peer)
                .await
                .unwrap();
            let mut bytes = [0; 16];
            let (length, source) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                server.recv_from(&mut bytes),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(&bytes[..length], b"DNS query");
            assert_eq!(source.ip().to_canonical(), ip);
            server.send_to(b"DNS answer", source).await.unwrap();
            let length = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client.recv_from(&mut bytes),
            )
            .await
            .unwrap()
            .unwrap()
            .0;
            assert_eq!(&bytes[..length], b"DNS answer");

            let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::new(ip, 0))
                .await
                .unwrap();
            let peer = listener.local_addr().unwrap();
            for destination in [
                peer,
                std::net::SocketAddr::new(
                    std::net::IpAddr::V6(match ip {
                        std::net::IpAddr::V4(ip) => ip.to_ipv6_mapped(),
                        _ => unreachable!(),
                    }),
                    peer.port(),
                ),
            ] {
                let client = crate::socket::tcp::create_tcp_socket(
                    destination,
                    &easytier_core::socket::tcp::TcpBindOptions::default(),
                )
                .await
                .unwrap();
                let mut client = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    client.connect(destination),
                )
                .await
                .unwrap()
                .unwrap_or_else(|error| panic!("protected TCP connect to {destination}: {error}"));
                assert_eq!(client.local_addr().unwrap().ip().to_canonical(), ip);
                let (mut server, _) = listener.accept().await.unwrap();
                client.write_all(b"reconnect").await.unwrap();
                let mut bytes = [0; 9];
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    server.read_exact(&mut bytes),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(&bytes, b"reconnect");
            }
            let mapped = std::net::SocketAddr::new(
                std::net::IpAddr::V6(match ip {
                    std::net::IpAddr::V4(ip) => ip.to_ipv6_mapped(),
                    _ => unreachable!(),
                }),
                peer.port(),
            );
            // The UDP server is still alive; use its original port for the
            // dual-stack socket's mapped IPv4 datagram.
            let mapped =
                std::net::SocketAddr::new(mapped.ip(), server.local_addr().unwrap().port());
            let dual_stack = crate::socket::udp::create_udp_socket(
                &easytier_core::socket::udp::UdpBindOptions::direct_connect()
                    .with_local_addr(Some("[::]:0".parse().unwrap())),
            )
            .await
            .unwrap();
            dual_stack.send_to(b"mapped DNS", mapped).await.unwrap();
            let length = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                server.recv_from(&mut bytes),
            )
            .await
            .unwrap()
            .unwrap()
            .0;
            assert_eq!(&bytes[..length], b"mapped DNS");
        }
    }
    #[test]
    fn bypass_bit_does_not_modify_external_mark_rules() {
        assert_eq!(
            choose_bypass_bit(0x8000_0042, [0x4000_0000, 0x0000_00ff]).unwrap(),
            0x2000_0000
        );
        assert!(choose_bypass_bit(0, [u32::MAX]).is_err());
        let original = 0x42;
        let bit = choose_bypass_bit(original, [0xff]).unwrap();
        assert_eq!((original | bit) & 0xff, original);
        assert_eq!((original | bit) & !bit, original);
    }

    #[test]
    fn registration_does_not_keep_closed_sockets_alive_or_reuse_borrowed_handles() {
        if !available() {
            return;
        }
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        let registration = register(&socket).unwrap().unwrap();
        let weak = Arc::downgrade(&registration);
        drop(socket);
        assert!(
            weak.upgrade().is_some(),
            "live socket owner keeps the duplicate valid"
        );
        drop(registration);
        assert!(
            weak.upgrade().is_none(),
            "registry must only hold a weak lifetime registration"
        );
    }
}
