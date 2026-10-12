use std::net::Ipv4Addr;

use super::{Error, IfConfiguerTrait, cidr_to_subnet_mask, run_shell_cmd};
use async_trait::async_trait;
use cidr::{Ipv4Inet, Ipv6Inet};

pub struct MacIfConfiger {}
#[async_trait]
impl IfConfiguerTrait for MacIfConfiger {
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
        let output = tokio::process::Command::new("route")
            .arg("-n")
            .arg("get")
            .arg(if ipv6 { "-inet6" } else { "-inet" })
            .arg("default")
            .output()
            .await
            .map_err(|e| Error::AnyhowError(anyhow::anyhow!("{e}")))?;
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        let mut gateway = None;
        let mut interface = None;
        for line in text.lines() {
            let mut parts = line.trim().split(':');
            match parts.next().map(str::trim) {
                Some("gateway") => {
                    gateway = parts.next().map(str::trim).and_then(|g| g.parse().ok());
                }
                Some("interface") => {
                    interface = parts.next().map(str::trim).map(ToString::to_string);
                }
                _ => {}
            }
        }
        if let Some(exclude) = exclude_interface
            && interface.as_deref() == Some(exclude)
        {
            return Ok(None);
        }
        Ok(gateway.map(|gateway| super::DefaultRouteInfo {
            gateway,
            interface_name: interface.unwrap_or_default(),
        }))
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
