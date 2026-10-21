//! Process-owned native socket registrations and desktop routing bypass.
//!
//! A registration owns a duplicate handle, never a borrowed raw handle. The
//! registry holds only weak references, so fd reuse cannot protect an unrelated
//! socket and TCP close is delayed only until its last reader/writer is dropped.

#[cfg(target_os = "macos")]
use nix::libc;
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex, OnceLock, Weak},
};

#[derive(Debug)]
pub(crate) struct SocketRegistration {
    socket: socket2::Socket,
    namespace: u64,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    io_lock: Mutex<()>,
}

#[derive(Clone)]
struct Protection {
    bit: u32,
    #[cfg(any(not(target_os = "linux"), test))]
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
        #[cfg(any(target_os = "windows", target_os = "macos"))]
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
        validate_destination_scope(&protection, destination)?;
        let ipv6 = destination.ip().to_canonical().is_ipv6();
        let previous = get_scope(socket, ipv6)?;
        let source_interface = source
            .map(|source| local_interface(&protection, source))
            .transpose()?
            .flatten();
        let local_interface = socket
            .local_addr()?
            .as_socket()
            .map(|addr| local_interface(&protection, addr.ip()))
            .transpose()?
            .flatten();
        let index = source_interface
            .or(local_interface)
            .or_else(|| native_interface(&protection, previous))
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
        Ok(result)
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
        #[cfg(any(target_os = "windows", target_os = "macos"))] interfaces: Vec<
            network_interface::NetworkInterface,
        >,
    ) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let _ = defaults;
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
            #[cfg(any(not(target_os = "linux"), test))]
            defaults,
            #[cfg(any(target_os = "windows", target_os = "macos", test))]
            interface_routes: {
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                {
                    interface_routes(interfaces)
                }
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                {
                    Arc::new([])
                }
            },
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

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub(crate) fn refresh_routes(
        &mut self,
        defaults: [Option<u32>; 2],
        interfaces: Vec<network_interface::NetworkInterface>,
    ) -> io::Result<()> {
        let routes = interface_routes(interfaces);
        let mut state = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = state.active.get_mut(&self.namespace) else {
            return Ok(());
        };
        active.defaults = defaults;
        active.interface_routes = routes;
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

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn interface_routes(
    interfaces: Vec<network_interface::NetworkInterface>,
) -> Arc<[(easytier_core::config::vpn::VpnCidr, u32)]> {
    interfaces
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
                    (std::net::IpAddr::V4(ip), std::net::IpAddr::V4(mask)) => std::net::IpAddr::V4(
                        std::net::Ipv4Addr::from(u32::from(ip) & u32::from(mask)),
                    ),
                    (std::net::IpAddr::V6(ip), std::net::IpAddr::V6(mask)) => std::net::IpAddr::V6(
                        std::net::Ipv6Addr::from(u128::from(ip) & u128::from(mask)),
                    ),
                    _ => return None,
                };
                let cidr = format!("{network}/{prefix}").parse().ok()?;
                Some((cidr, interface.index))
            })
        })
        .collect::<Vec<_>>()
        .into()
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
        return native_interface(protection, address.scope_id());
    }
    let destination = destination.ip().to_canonical();
    protection
        .interface_routes
        .iter()
        .filter(|(cidr, _)| cidr.contains(&destination))
        .max_by_key(|(cidr, _)| cidr.prefix_len())
        .map(|(_, index)| *index)
        .or_else(|| {
            protection.defaults[usize::from(destination.is_ipv6())]
                .and_then(|index| native_interface(protection, index))
        })
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn native_interface(protection: &Protection, index: u32) -> Option<u32> {
    // The snapshot contains eligible uplinks and loopback, never the capture
    // TUN. Default routes are also trusted because they were resolved by the
    // host route provider with the capture interface explicitly excluded. A
    // third-party VPN adapter may be the default egress while having no
    // physical MAC, so it is intentionally absent from the filtered interface
    // snapshot but must remain usable as a native underlay.
    let is_default = protection
        .defaults
        .iter()
        .flatten()
        .any(|default| *default == index);
    (index != 0 && (is_default || protection.interface_routes.iter().any(|(_, i)| *i == index)))
        .then_some(index)
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn validate_destination_scope(
    protection: &Protection,
    destination: std::net::SocketAddr,
) -> io::Result<()> {
    if let std::net::SocketAddr::V6(address) = destination
        && address.scope_id() != 0
        && native_interface(protection, address.scope_id()).is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::NetworkUnreachable,
            "VPN bypass destination scope is not an eligible native interface",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn local_interface(protection: &Protection, ip: std::net::IpAddr) -> io::Result<Option<u32>> {
    let ip = ip.to_canonical();
    if ip.is_unspecified() {
        return Ok(None);
    }
    crate::tunnel::common::get_interface_name_by_ip(&ip)
        .and_then(|name| interface_index(&name))
        .and_then(|index| native_interface(protection, index))
        .map(Some)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NetworkUnreachable,
                format!("VPN bypass source {ip} is not on an eligible native interface"),
            )
        })
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn apply_scope(
    socket: &socket2::Socket,
    protection: &Protection,
    destination: Option<std::net::SocketAddr>,
) -> io::Result<()> {
    if let Some(destination) = destination {
        validate_destination_scope(protection, destination)?;
    }
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
    let explicit = local_interface(protection, addr.ip())?;
    let index = explicit
        .or_else(|| native_interface(protection, previous))
        .or_else(|| {
            destination.and_then(|destination| destination_interface(protection, destination))
        })
        .or_else(|| {
            protection.defaults[usize::from(ipv6)]
                .and_then(|index| native_interface(protection, index))
        })
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
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
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
                ("10.1.0.0/16", 1),
                ("2001:db8:1::/64", 2),
                ("127.0.0.0/8", 3),
                ("::1/128", 3),
                ("192.168.1.0/24", 4),
                ("192.168.1.128/25", 5),
                ("fd12::/64", 6),
                ("fe80::/64", 7),
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
        for destination in ["[fe80::1%99]:53", "[ff02::1%99]:53"] {
            let destination = destination.parse().unwrap();
            assert_eq!(destination_interface(&protection, destination), None);
            assert_eq!(
                validate_destination_scope(&protection, destination)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::NetworkUnreachable
            );
        }
        assert_eq!(native_interface(&protection, 99), None);
        assert_eq!(native_interface(&protection, 0), None);
        assert_eq!(native_interface(&protection, 1), Some(1));
        assert_eq!(native_interface(&protection, 2), Some(2));
    }

    #[test]
    fn default_scope_accepts_virtual_underlay_without_filtered_route() {
        let protection = Protection {
            bit: 0,
            defaults: [Some(8), Some(9)],
            interface_routes: [("127.0.0.0/8", 3), ("::1/128", 3)]
                .into_iter()
                .map(|(cidr, index)| (cidr.parse().unwrap(), index))
                .collect::<Vec<_>>()
                .into(),
            owners: 1,
        };
        assert_eq!(native_interface(&protection, 8), Some(8));
        assert_eq!(native_interface(&protection, 9), Some(9));
        assert_eq!(native_interface(&protection, 99), None);
        assert_eq!(native_interface(&protection, 0), None);
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
        let early_server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let early_remote = early_server.local_addr().unwrap();
        let early_dial = crate::socket::tcp::create_tcp_socket(
            early_remote,
            &easytier_core::socket::tcp::TcpBindOptions::default(),
        )
        .await
        .unwrap();
        let mut established = crate::socket::tcp::create_tcp_socket(
            early_remote,
            &easytier_core::socket::tcp::TcpBindOptions::default(),
        )
        .await
        .unwrap()
        .connect(early_remote)
        .await
        .unwrap();
        let (mut established_peer, _) = early_server.accept().await.unwrap();
        let early_udp = crate::socket::udp::create_udp_socket(
            &easytier_core::socket::udp::UdpBindOptions::direct_connect(),
        )
        .await
        .unwrap();
        // An invalid fallback makes this test prove connected/loopback lookup
        // was used, rather than accidentally succeeding on a default route.
        let mut capture = Cleanup(
            DesktopProtectionLease::acquire(
                0,
                0,
                [Some(u32::MAX), Some(u32::MAX)],
                interfaces.clone(),
            )
            .unwrap(),
        );
        established
            .write_all(b"live across takeover")
            .await
            .unwrap();
        let mut established_bytes = [0; 20];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            established_peer.read_exact(&mut established_bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&established_bytes, b"live across takeover");
        // Sockets registered before takeover must see the complete snapshot as
        // soon as protection becomes active; no follow-up refresh is required.
        let _early_client = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            early_dial.connect(early_remote),
        )
        .await
        .unwrap()
        .unwrap();
        let (_early_accepted, _) = early_server.accept().await.unwrap();
        let early_receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        early_udp
            .send_to(b"existing socket", early_receiver.local_addr().unwrap())
            .await
            .unwrap();
        let mut early_bytes = [0; 32];
        let (length, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            early_receiver.recv_from(&mut early_bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&early_bytes[..length], b"existing socket");
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

        // Treat a real non-loopback adapter as the capture TUN by omitting it
        // from the eligible snapshot. This exercises the native socket options
        // without installing routes or requiring an administrator token.
        if let Some((excluded, ip)) = interfaces.iter().filter(|i| !i.internal).find_map(|i| {
            i.addr.iter().find_map(|a| match a.ip() {
                std::net::IpAddr::V4(ip) if ip.is_private() => Some((i.index, ip)),
                _ => None,
            })
        }) {
            let eligible: Vec<_> = interfaces.iter().filter(|i| i.internal).cloned().collect();
            let loopback = eligible.first().map(|interface| interface.index);
            assert!(loopback.is_some());
            capture
                .0
                .refresh_routes([loopback, loopback], eligible)
                .unwrap();
            let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let remote = server.local_addr().unwrap();
            let socket = crate::socket::udp::create_udp_socket(
                &easytier_core::socket::udp::UdpBindOptions::direct_connect(),
            )
            .await
            .unwrap();
            set_scope(&socket2::SockRef::from(&*socket), false, excluded).unwrap();
            socket.send_to(b"bypass stale scope", remote).await.unwrap();
            let mut bytes = [0; 32];
            let (length, _) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                server.recv_from(&mut bytes),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(&bytes[..length], b"bypass stale scope");
            assert_eq!(
                get_scope(&socket2::SockRef::from(&*socket), false).unwrap(),
                excluded
            );
            let error = socket
                .send_to_with_src_ip(ip.into(), remote, b"no TUN source")
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NetworkUnreachable);

            let bound = crate::socket::udp::create_udp_socket(
                &easytier_core::socket::udp::UdpBindOptions::direct_connect()
                    .with_local_addr(Some(std::net::SocketAddr::new(ip.into(), 0))),
            )
            .await
            .unwrap();
            assert_eq!(
                bound
                    .send_to(b"no bound TUN", remote)
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::NetworkUnreachable
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let remote = listener.local_addr().unwrap();
            let dial = crate::socket::tcp::create_tcp_socket(
                remote,
                &easytier_core::socket::tcp::TcpBindOptions::default(),
            )
            .await
            .unwrap();
            set_scope(&socket2::SockRef::from(&*dial), false, excluded).unwrap();
            let _client =
                tokio::time::timeout(std::time::Duration::from_secs(2), dial.connect(remote))
                    .await
                    .unwrap()
                    .unwrap();
            let (_accepted, _) = listener.accept().await.unwrap();
            let bound = crate::socket::tcp::create_tcp_socket(
                remote,
                &easytier_core::socket::tcp::TcpBindOptions::default()
                    .with_local_addr(Some(std::net::SocketAddr::new(ip.into(), 0))),
            )
            .await
            .unwrap();
            assert_eq!(
                bound.connect(remote).await.unwrap_err().kind(),
                io::ErrorKind::NetworkUnreachable
            );
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
