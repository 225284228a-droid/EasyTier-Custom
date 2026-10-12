use std::net::Ipv4Addr;

use super::{Error, IfConfiguerTrait, cidr_to_subnet_mask, run_shell_cmd};
use async_trait::async_trait;
use cidr::{Ipv4Inet, Ipv6Inet};

pub struct MacIfConfiger {}

#[async_trait]
impl IfConfiguerTrait for MacIfConfiger {
    // Kernel NAT66 via pf was reverted: the anchor executor shipped in
    // b9ad4cff never compiled (missing methods). macOS keeps the trait
    // default (`sync_ipv6_exit_forwarding` -> unsupported); IPv4 exit
    // forwarding is unaffected.

    async fn add_route_via_gateway(
        &self,
        gateway: std::net::IpAddr,
        destination: std::net::IpAddr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        match gateway {
            std::net::IpAddr::V4(gw) => run_shell_cmd(
                format!(
                    "route -n add -net {} -netmask {} {}",
                    destination,
                    cidr_to_subnet_mask(cidr_prefix),
                    gw
                )
                .as_str(),
            )
            .await,
            std::net::IpAddr::V6(gw) => run_shell_cmd(
                format!("route -n add -inet6 {}/{} {}", destination, cidr_prefix, gw).as_str()
            )
            .await,
        }
    }

    async fn remove_route_via_gateway(
        &self,
        gateway: std::net::IpAddr,
        destination: std::net::IpAddr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        match gateway {
            std::net::IpAddr::V4(_gw) => run_shell_cmd(
                format!(
                    "route -n delete -net {} -netmask {}",
                    destination,
                    cidr_to_subnet_mask(cidr_prefix)
                )
                .as_str(),
            )
            .await,
            std::net::IpAddr::V6(_gw) => run_shell_cmd(
                format!("route -n delete -inet6 {}/{}", destination, cidr_prefix).as_str()
            )
            .await,
        }
    }

    async fn default_route(
        &self,
        ipv6: bool,
        exclude_interface: Option<&str>,
    ) -> Result<Option<super::DefaultRouteInfo>, Error> {
        // Scan the routing table instead of `route get default`: a route
        // lookup for 0.0.0.0 is captured by the VPN's 0.0.0.0/1 takeover
        // route, while the physical default entry remains visible in the
        // table itself.
        let output = tokio::process::Command::new("netstat")
            .arg("-rn")
            .arg("-f")
            .arg(if ipv6 { "inet6" } else { "inet" })
            .output()
            .await
            .map_err(|e| Error::AnyhowError(anyhow::anyhow!("{e}")))?;
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(parse_netstat_default_route(&text, exclude_interface))
    }

    async fn add_ipv4_route(
        &self,
        name: &str,
        address: Ipv4Addr,
        cidr_prefix: u8,
        cost: Option<i32>,
    ) -> Result<(), Error> {
        run_shell_cmd(
            format!(
                "route -n add {} -netmask {} -interface {} -hopcount {}",
                address,
                cidr_to_subnet_mask(cidr_prefix),
                name,
                cost.unwrap_or(7)
            )
            .as_str(),
        )
        .await
    }

    async fn remove_ipv4_route(
        &self,
        name: &str,
        address: Ipv4Addr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        run_shell_cmd(
            format!(
                "route -n delete {} -netmask {} -interface {}",
                address,
                cidr_to_subnet_mask(cidr_prefix),
                name
            )
            .as_str(),
        )
        .await
    }

    async fn add_ipv4_ip(
        &self,
        name: &str,
        address: Ipv4Addr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        run_shell_cmd(
            format!(
                "ifconfig {} {:?}/{:?} {:?} up",
                name, address, cidr_prefix, address,
            )
            .as_str(),
        )
        .await
    }

    async fn set_link_status(&self, name: &str, up: bool) -> Result<(), Error> {
        run_shell_cmd(format!("ifconfig {} {}", name, if up { "up" } else { "down" }).as_str())
            .await
    }

    async fn remove_ip(&self, name: &str, ip: Option<Ipv4Inet>) -> Result<(), Error> {
        if let Some(ip) = ip {
            run_shell_cmd(format!("ifconfig {} inet {} delete", name, ip.address()).as_str()).await
        } else {
            run_shell_cmd(format!("ifconfig {} inet delete", name).as_str()).await
        }
    }

    async fn set_mtu(&self, name: &str, mtu: u32) -> Result<(), Error> {
        run_shell_cmd(format!("ifconfig {} mtu {}", name, mtu).as_str()).await
    }

    async fn add_ipv6_ip(
        &self,
        name: &str,
        address: std::net::Ipv6Addr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        run_shell_cmd(format!("ifconfig {} inet6 {}/{} add", name, address, cidr_prefix).as_str())
            .await
    }

    async fn remove_ipv6(&self, name: &str, ip: Option<Ipv6Inet>) -> Result<(), Error> {
        if let Some(ip) = ip {
            run_shell_cmd(format!("ifconfig {} inet6 {} delete", name, ip.address()).as_str()).await
        } else {
            // Remove all IPv6 addresses is more complex on macOS, just succeed
            Ok(())
        }
    }

    async fn add_ipv6_route(
        &self,
        name: &str,
        address: std::net::Ipv6Addr,
        cidr_prefix: u8,
        cost: Option<i32>,
    ) -> Result<(), Error> {
        let cmd = if let Some(cost) = cost {
            format!(
                "route -n add -inet6 {}/{} -interface {} -hopcount {}",
                address, cidr_prefix, name, cost
            )
        } else {
            format!(
                "route -n add -inet6 {}/{} -interface {}",
                address, cidr_prefix, name
            )
        };
        run_shell_cmd(cmd.as_str()).await
    }

    async fn remove_ipv6_route(
        &self,
        name: &str,
        address: std::net::Ipv6Addr,
        cidr_prefix: u8,
    ) -> Result<(), Error> {
        run_shell_cmd(
            format!(
                "route -n delete -inet6 {}/{} -interface {}",
                address, cidr_prefix, name
            )
            .as_str(),
        )
        .await
    }
}

/// Parses `netstat -rn` output for the default route of one family. Rows are
/// `Destination Gateway Flags Netif [Expire]`; interface-scoped or link-level
/// defaults whose gateway is not a plain address are skipped, as is the
/// excluded interface (the EasyTier TUN).
fn parse_netstat_default_route(
    text: &str,
    exclude_interface: Option<&str>,
) -> Option<super::DefaultRouteInfo> {
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.first().copied() != Some("default") || tokens.len() < 4 {
            continue;
        }
        let Some(gateway) = tokens[1].split('%').next().and_then(|g| g.parse().ok()) else {
            // `link#N` gateways (interface routes) have no usable next hop.
            continue;
        };
        let interface = tokens[3];
        if let Some(exclude) = exclude_interface
            && interface == exclude
        {
            continue;
        }
        return Some(super::DefaultRouteInfo {
            gateway,
            interface_name: interface.to_owned(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_netstat_default_route;

    const MACOS_MIXED: &str = "\
Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
default            192.168.1.1        UGScg                 en0
10.0.0/8           link#4             UCS                   en0
127                127.0.0.1          UCS                   lo0

Internet6:
Destination        Gateway            Flags         Netif Expire
default            fe80::1%en0        UGcIg            en0
default            link#12            UCSIg            utun8
::1                ::1                UHL              lo0
";

    #[test]
    fn parses_v4_default_with_gateway() {
        let route = parse_netstat_default_route(MACOS_MIXED, None).unwrap();
        assert_eq!(route.gateway, "192.168.1.1".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(route.interface_name, "en0");
    }

    #[test]
    fn v6_zone_suffix_is_stripped() {
        let v6 = "\
Internet6:
Destination        Gateway            Flags         Netif Expire
default            fe80::1%en0        UGcIg            en0
";
        let route = parse_netstat_default_route(v6, None).unwrap();
        assert_eq!(
            route.gateway,
            "fe80::1".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn excluded_interface_is_skipped_and_link_gateways_ignored() {
        // With en0 excluded, the v4 match must fail (only link# row remains
        // for the other interfaces in this table).
        let table = "\
Internet:
Destination        Gateway            Flags     Netif Expire
default            192.168.1.1        UGS       en0
default            link#9             UCSI      utun8
";
        assert!(parse_netstat_default_route(table, Some("en0")).is_none());
        let route = parse_netstat_default_route(table, None).unwrap();
        assert_eq!(route.interface_name, "en0");
    }
}
