//! Host VPN capture routes are compiled centrally and installed with native socket bypass.
#[cfg(target_os = "windows")]
use std::net::Ipv6Addr;
use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr},
};
#[path = "vpn_route_manager.rs"]
mod manager;
pub use manager::{VpnRouteManagerContext, VpnRouteStatusCell, run_vpn_route_manager};

fn is_pinnable_dns(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback() && !v4.is_broadcast() && !v4.is_multicast() && !v4.is_unspecified()
        }
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_multicast() && !v6.is_unspecified(),
    }
}

#[cfg(any(not(target_os = "windows"), test))]
fn parse_resolver_dns_servers(content: &str) -> BTreeSet<IpAddr> {
    content
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? != "nameserver" {
                return None;
            }
            let address = fields.next()?;
            let (address, scope) = address
                .split_once('%')
                .map_or((address, None), |(address, scope)| (address, Some(scope)));
            let ip = address.parse::<IpAddr>().ok()?;
            if scope.is_some_and(|scope| !ip.is_ipv6() || scope.is_empty() || scope.contains('%')) {
                return None;
            }
            Some(ip.to_canonical())
        })
        .collect()
}

#[cfg(any(not(target_os = "windows"), test))]
fn list_resolver_dns_servers(
    namespace: Option<&str>,
    mut read: impl FnMut(&std::path::Path) -> Option<String>,
) -> Vec<IpAddr> {
    let resolver = namespace
        .and_then(|namespace| {
            read(
                &std::path::Path::new("/etc/netns")
                    .join(namespace)
                    .join("resolv.conf"),
            )
        })
        .or_else(|| read(std::path::Path::new("/etc/resolv.conf")))
        .unwrap_or_default();
    let mut servers = parse_resolver_dns_servers(&resolver);
    if servers.iter().any(|ip| {
        matches!(ip, IpAddr::V4(ip) if *ip == Ipv4Addr::new(127, 0, 0, 53) || *ip == Ipv4Addr::new(127, 0, 0, 54))
    }) {
        // The stub's forwarding sockets belong to systemd-resolved. Capture
        // its published upstreams as user DNS destinations, independently of
        // the protected resolver used by EasyTier's own bootstrap traffic.
        if let Some(uplinks) = read(std::path::Path::new("/run/systemd/resolve/resolv.conf")) {
            servers.extend(parse_resolver_dns_servers(&uplinks));
        }
    }
    servers.into_iter().filter(is_pinnable_dns).collect()
}

/// Lists resolver destinations in the instance's network namespace.
fn list_system_dns_servers(net_ns: &crate::common::netns::NetNS) -> Vec<IpAddr> {
    let mut servers: BTreeSet<IpAddr> = BTreeSet::new();

    #[cfg(target_os = "windows")]
    {
        let _ = net_ns;
        servers.extend(list_windows_dns_servers());
    }

    #[cfg(not(target_os = "windows"))]
    {
        #[cfg(target_os = "linux")]
        let namespace = net_ns.name();
        #[cfg(not(target_os = "linux"))]
        let namespace: Option<String> = {
            let _ = net_ns;
            None
        };
        servers.extend(list_resolver_dns_servers(namespace.as_deref(), |path| {
            std::fs::read_to_string(path).ok()
        }));
    }

    servers.into_iter().filter(is_pinnable_dns).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_parser_accepts_whitespace_comments_and_exact_nameserver_tokens() {
        let servers = parse_resolver_dns_servers(
            "  nameserver\t1.1.1.1 # comment\nnameserver 2001:4860:4860::8888\nnameserver fe80::53%physical # scoped DNS\nnameserver 1.1.1.1\nnameserver [invalid]\nnameserver 192.168.1.1%physical\nnameserver-extra 9.9.9.9\n# nameserver 8.8.8.8\n",
        );
        assert_eq!(
            servers,
            ["1.1.1.1", "2001:4860:4860::8888", "fe80::53"]
                .map(|ip| ip.parse().unwrap())
                .into()
        );
    }

    #[test]
    fn system_dns_capture_prefers_target_namespace_resolver() {
        let mut reads = Vec::new();
        let servers = list_resolver_dns_servers(Some("vpn-test"), |path| {
            reads.push(path.to_owned());
            (path == std::path::Path::new("/etc/netns/vpn-test/resolv.conf"))
                .then(|| "nameserver 192.168.1.1\n".to_owned())
        });
        assert_eq!(servers, ["192.168.1.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(
            reads,
            [std::path::PathBuf::from("/etc/netns/vpn-test/resolv.conf")]
        );
    }

    #[test]
    fn system_dns_capture_falls_back_to_host_resolver_when_namespace_file_is_missing() {
        let mut reads = Vec::new();
        let servers = list_resolver_dns_servers(Some("vpn-test"), |path| {
            reads.push(path.to_owned());
            (path == std::path::Path::new("/etc/resolv.conf"))
                .then(|| "nameserver 9.9.9.9\n".to_owned())
        });
        assert_eq!(servers, ["9.9.9.9".parse::<IpAddr>().unwrap()]);
        assert_eq!(
            reads,
            [
                std::path::PathBuf::from("/etc/netns/vpn-test/resolv.conf"),
                std::path::PathBuf::from("/etc/resolv.conf")
            ]
        );
    }

    #[test]
    fn system_dns_capture_merges_systemd_stub_uplinks_without_losing_other_servers() {
        let servers = list_resolver_dns_servers(Some("vpn-test"), |path| {
            if path == std::path::Path::new("/etc/netns/vpn-test/resolv.conf") {
                Some(
                    "nameserver 127.0.0.53\nnameserver 127.0.0.54\nnameserver 1.1.1.1\n".to_owned(),
                )
            } else if path == std::path::Path::new("/run/systemd/resolve/resolv.conf") {
                Some("nameserver 192.168.1.1\nnameserver 1.1.1.1\nnameserver 2001:4860:4860::8888\nnameserver ::1\nnameserver fd12::53\n".to_owned())
            } else {
                None
            }
        });
        assert_eq!(
            servers,
            ["1.1.1.1", "192.168.1.1", "2001:4860:4860::8888", "fd12::53"]
                .map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn system_dns_capture_without_stub_does_not_merge_unrelated_uplinks() {
        let servers = list_resolver_dns_servers(None, |path| {
            assert_eq!(path, std::path::Path::new("/etc/resolv.conf"));
            Some("nameserver 8.8.8.8\n".to_owned())
        });
        assert_eq!(servers, ["8.8.8.8".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn private_dns_capture_defers_to_policy_switch_and_real_route_exclusions() {
        use easytier_core::config::vpn::{
            VpnConfig, VpnRuleAction, VpnRuleConfig,
            routes::{VpnRouteExclusions, compile_vpn_routes},
        };
        let private = ["192.168.1.1", "169.254.1.53", "fd12::53", "fe80::53"]
            .map(|ip| ip.parse::<IpAddr>().unwrap());
        let public = "1.1.1.1".parse::<IpAddr>().unwrap();
        let overlay = "10.66.0.1".parse::<IpAddr>().unwrap();
        let physical = "192.168.2.53".parse::<IpAddr>().unwrap();
        let resolver = private
            .into_iter()
            .chain([public, overlay, physical])
            .map(|ip| format!("nameserver {ip}\n"))
            .collect::<String>();
        let dns = list_resolver_dns_servers(None, |_| Some(resolver.clone()));
        assert_eq!(
            dns.len(),
            7,
            "private/link-local inputs must reach the compiler"
        );
        let exclusions = VpnRouteExclusions {
            overlay: vec!["10.66.0.0/24".parse().unwrap()],
            physical: vec!["192.168.2.0/24".parse().unwrap()],
        };
        for ignore_private in [false, true] {
            let config = VpnConfig {
                dns_protect: true,
                ipv6_protect: false,
                ignore_private,
                rules: vec![VpnRuleConfig {
                    cidr: "203.0.113.0/24".parse().unwrap(),
                    action: VpnRuleAction::Exit,
                    exits: vec!["10.66.0.2".parse().unwrap()],
                    priority: 0,
                }],
                ..Default::default()
            };
            let plan = compile_vpn_routes(&config, &exclusions, &dns);
            for ip in private {
                assert_eq!(
                    plan.regions
                        .iter()
                        .any(|region| region.cidr.contains(&ip)
                            && region.action == VpnRuleAction::Block),
                    !ignore_private,
                    "{ip} ignore_private={ignore_private}"
                );
                assert_eq!(
                    plan.capture_routes.iter().any(|cidr| cidr.contains(&ip)),
                    !ignore_private,
                    "{ip} capture ignore_private={ignore_private}"
                );
            }
            assert!(plan.regions.iter().any(
                |region| region.cidr.contains(&public) && region.action == VpnRuleAction::Block
            ));
            for exempt in [overlay, physical] {
                assert!(
                    !plan
                        .regions
                        .iter()
                        .any(|region| region.cidr.contains(&exempt)),
                    "real exclusion {exempt}"
                );
            }
            assert!(
                !plan
                    .capture_routes
                    .iter()
                    .any(|cidr| cidr.contains(&physical))
            );
        }
    }
}

/// Lists DNS servers of all up adapters through GetAdaptersAddresses.
/// Replaces the previous `ipconfig /all` text scrape, which only matched
/// English/Chinese labels and missed IPv6 servers on wrapped lines.
#[cfg(target_os = "windows")]
fn list_windows_dns_servers() -> Vec<IpAddr> {
    use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6, SOCKADDR, SOCKADDR_IN};

    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_FRIENDLY_NAME;
    let mut size: u32 = 16 * 1024;
    let mut buffer = vec![0u8; size as usize];
    let table_ptr;
    loop {
        let result = unsafe {
            GetAdaptersAddresses(
                0, // AF_UNSPEC: both families
                flags,
                None,
                Some(buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if result == ERROR_BUFFER_OVERFLOW.0 {
            buffer.resize(size as usize, 0);
            continue;
        }
        if result != 0 {
            tracing::warn!(result, "GetAdaptersAddresses failed");
            return Vec::new();
        }
        table_ptr = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        break;
    }

    let mut servers = BTreeSet::new();
    let mut adapter = table_ptr;
    while !adapter.is_null() {
        let adapter_ref = unsafe { &*adapter };
        if adapter_ref.OperStatus == IfOperStatusUp {
            let mut entry = adapter_ref.FirstDnsServerAddress;
            while !entry.is_null() {
                let entry_ref = unsafe { &*entry };
                let sockaddr = entry_ref.Address.lpSockaddr as *const SOCKADDR;
                if !sockaddr.is_null() {
                    let family = unsafe { (*sockaddr).sa_family };
                    let ip = unsafe {
                        if family == AF_INET {
                            let v4 = &*(sockaddr as *const SOCKADDR_IN);
                            Some(IpAddr::V4(Ipv4Addr::new(
                                v4.sin_addr.S_un.S_un_b.s_b1,
                                v4.sin_addr.S_un.S_un_b.s_b2,
                                v4.sin_addr.S_un.S_un_b.s_b3,
                                v4.sin_addr.S_un.S_un_b.s_b4,
                            )))
                        } else if family == AF_INET6 {
                            let v6 = &*(sockaddr
                                as *const windows::Win32::Networking::WinSock::SOCKADDR_IN6);
                            Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.u.Byte)))
                        } else {
                            None
                        }
                    };
                    if let Some(ip) = ip {
                        servers.insert(ip);
                    }
                }
                entry = entry_ref.Next;
            }
        }
        adapter = adapter_ref.Next;
    }
    // The API wrote into our own buffer; there is nothing to free.
    drop(buffer);
    servers.into_iter().filter(is_pinnable_dns).collect()
}
