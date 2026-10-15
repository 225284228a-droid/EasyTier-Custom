#![cfg_attr(not(feature = "tun"), allow(dead_code))]

#[cfg(any(
    all(target_os = "macos", not(feature = "macos-ne")),
    target_os = "freebsd"
))]
mod darwin;
#[cfg(all(target_os = "linux", feature = "linux-netlink"))]
mod netlink;
#[cfg(all(target_os = "linux", feature = "linux-netlink"))]
mod netlink_wire;
#[cfg(target_os = "windows")]
mod win;
#[cfg(target_os = "windows")]
mod windows;

use std::net::{Ipv4Addr, Ipv6Addr};

use async_trait::async_trait;
use cidr::{Ipv4Inet, Ipv6Inet};
#[cfg(any(
    all(target_os = "macos", not(feature = "macos-ne")),
    target_os = "freebsd"
))]
use tokio::process::Command;

use super::error::Error;

/// The system's current default route for one address family, captured
/// before a VPN takes over routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultRouteInfo {
    pub gateway: std::net::IpAddr,
    pub interface_name: String,
}

#[async_trait]
pub trait IfConfiguerTrait: Send + Sync {
    /// Adds a route to `destination` through `gateway` (the pre-VPN default
    /// gateway). Used for VPN "direct" exclusion rules and for protecting
    /// EasyTier's own underlay connections from the tunneled default route.
    async fn add_route_via_gateway(
        &self,
        _gateway: std::net::IpAddr,
        _destination: std::net::IpAddr,
        _cidr_prefix: u8,
    ) -> Result<(), Error> {
        Err(Error::AnyhowError(anyhow::anyhow!(
            "route via gateway is unsupported on this platform"
        )))
    }
    async fn remove_route_via_gateway(
        &self,
        _gateway: std::net::IpAddr,
        _destination: std::net::IpAddr,
        _cidr_prefix: u8,
    ) -> Result<(), Error> {
        Err(Error::AnyhowError(anyhow::anyhow!(
            "route via gateway is unsupported on this platform"
        )))
    }
    /// Turns kernel IPv6 exit forwarding (NAT66) for the multi-exit VPN on
    /// or off: mesh traffic from `mesh_prefix` leaving through any interface
    /// except the TUN gets masqueraded. Platforms without kernel NAT66 keep
    /// the default error; their exits relay v6 in the userspace proxy
    /// engines instead.
    async fn sync_ipv6_exit_forwarding(
        &self,
        _tun_ifname: &str,
        _mesh_prefix: Option<&str>,
        _enabled: bool,
    ) -> Result<(), Error> {
        Err(Error::AnyhowError(anyhow::anyhow!(
            "kernel NAT66 is unsupported on this platform"
        )))
    }
    /// Discovers the current default route. `None` when no default route of
    /// that family exists.
    async fn default_route(
        &self,
        _ipv6: bool,
        _exclude_interface: Option<&str>,
    ) -> Result<Option<DefaultRouteInfo>, Error> {
        Ok(None)
    }
    async fn add_ipv4_route(
        &self,
        _name: &str,
        _address: Ipv4Addr,
        _cidr_prefix: u8,
        _cost: Option<i32>,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn add_ipv4_route_with_source_hint(
        &self,
        name: &str,
        address: Ipv4Addr,
        cidr_prefix: u8,
        cost: Option<i32>,
        _source_hint: Option<Ipv4Addr>,
    ) -> Result<(), Error> {
        self.add_ipv4_route(name, address, cidr_prefix, cost).await
    }
    async fn remove_ipv4_route(
        &self,
        _name: &str,
        _address: Ipv4Addr,
        _cidr_prefix: u8,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn remove_ipv4_route_with_cost_and_source_hint(
        &self,
        name: &str,
        address: Ipv4Addr,
        cidr_prefix: u8,
        _cost: Option<i32>,
        _source_hint: Option<Ipv4Addr>,
    ) -> Result<(), Error> {
        self.remove_ipv4_route(name, address, cidr_prefix).await
    }
    async fn add_ipv4_ip(
        &self,
        _name: &str,
        _address: Ipv4Addr,
        _cidr_prefix: u8,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn add_ipv6_route(
        &self,
        _name: &str,
        _address: Ipv6Addr,
        _cidr_prefix: u8,
        _cost: Option<i32>,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn remove_ipv6_route(
        &self,
        _name: &str,
        _address: Ipv6Addr,
        _cidr_prefix: u8,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn add_ipv6_ip(
        &self,
        _name: &str,
        _address: Ipv6Addr,
        _cidr_prefix: u8,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn set_link_status(&self, _name: &str, _up: bool) -> Result<(), Error> {
        Ok(())
    }
    async fn remove_ip(&self, _name: &str, _ip: Option<Ipv4Inet>) -> Result<(), Error> {
        Ok(())
    }
    async fn remove_ipv6(&self, _name: &str, _ip: Option<Ipv6Inet>) -> Result<(), Error> {
        Ok(())
    }
    async fn wait_interface_show(&self, _name: &str) -> Result<(), Error> {
        return Ok(());
    }
    async fn set_mtu(&self, _name: &str, _mtu: u32) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(any(
    all(target_os = "macos", not(feature = "macos-ne")),
    target_os = "freebsd"
))]
fn cidr_to_subnet_mask(prefix_length: u8) -> Ipv4Addr {
    if prefix_length > 32 {
        panic!("Invalid CIDR prefix length");
    }

    let subnet_mask: u32 = (!0u32)
        .checked_shl(32 - u32::from(prefix_length))
        .unwrap_or(0);
    Ipv4Addr::new(
        ((subnet_mask >> 24) & 0xFF) as u8,
        ((subnet_mask >> 16) & 0xFF) as u8,
        ((subnet_mask >> 8) & 0xFF) as u8,
        (subnet_mask & 0xFF) as u8,
    )
}

#[cfg(any(
    all(target_os = "macos", not(feature = "macos-ne")),
    target_os = "freebsd"
))]
async fn run_shell_cmd(cmd: &str) -> Result<(), Error> {
    let cmd_out: std::process::Output;
    let stdout: String;
    let stderr: String;
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd_out = Command::new("cmd")
            .stdin(std::process::Stdio::null())
            .arg("/C")
            .arg(cmd)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .await?;
        stdout = crate::utils::string::utf8_or_gbk_to_string(cmd_out.stdout.as_slice());
        stderr = crate::utils::string::utf8_or_gbk_to_string(cmd_out.stderr.as_slice());
    };

    #[cfg(not(target_os = "windows"))]
    {
        cmd_out = Command::new("sh")
            .env("LC_ALL", "C")
            .arg("-c")
            .arg(cmd)
            .output()
            .await?;
        stdout = String::from_utf8_lossy(cmd_out.stdout.as_slice()).to_string();
        stderr = String::from_utf8_lossy(cmd_out.stderr.as_slice()).to_string();
    };

    let ec = cmd_out.status.code();
    let succ = cmd_out.status.success();
    tracing::info!(?cmd, ?ec, ?succ, ?stdout, ?stderr, "run shell cmd");

    if !cmd_out.status.success() {
        return Err(Error::ShellCommandError(stdout + &stderr));
    }
    Ok(())
}

#[cfg(any(
    all(target_os = "macos", not(feature = "macos-ne")),
    target_os = "freebsd",
    test
))]
fn ignore_removed_shell_cmd_not_found(result: Result<(), Error>) -> Result<(), Error> {
    match result {
        Err(Error::ShellCommandError(output))
            if !output.trim().is_empty()
                && output
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .all(|line| {
                        (line.starts_with("ifconfig: ")
                            && line.ends_with(": Can't assign requested address"))
                            || (line.starts_with("delete ") && line.ends_with(": not in table"))
                            || line == "route: route has not been found"
                            || line == "route: writing to routing socket: not in table"
                            || line == "route: writing to routing socket: No such process"
                    }) =>
        {
            Ok(())
        }
        other => other,
    }
}

#[derive(Default)]
pub struct DummyIfConfiger {}
#[async_trait]
impl IfConfiguerTrait for DummyIfConfiger {}

#[cfg(all(target_os = "linux", feature = "linux-netlink"))]
pub type IfConfiger = netlink::NetlinkIfConfiger;
#[cfg(all(target_os = "linux", not(feature = "linux-netlink")))]
pub type IfConfiger = DummyIfConfiger;

#[cfg(any(
    all(target_os = "macos", not(feature = "macos-ne")),
    target_os = "freebsd"
))]
pub type IfConfiger = darwin::MacIfConfiger;

#[cfg(target_os = "windows")]
pub type IfConfiger = windows::WindowsIfConfiger;

#[cfg(not(any(
    all(target_os = "macos", not(feature = "macos-ne")),
    target_os = "linux",
    target_os = "windows",
    target_os = "freebsd",
)))]
pub type IfConfiger = DummyIfConfiger;

#[cfg(target_os = "windows")]
pub use windows::RegistryManager;

#[cfg(test)]
mod tests {
    use super::{Error, ignore_removed_shell_cmd_not_found};

    #[test]
    fn removed_shell_cmd_ignores_missing_addresses_and_routes() {
        for output in [
            "ifconfig: ioctl (SIOCDIFADDR): Can't assign requested address\n",
            "ifconfig: ioctl (SIOCDIFADDR_IN6): Can't assign requested address\n",
            "delete net 10.0.0.0: gateway tun0: not in table\n",
            "delete net 10.0.0.0: gateway tun0 fib 0: not in table\nroute: route has not been found\n",
            "delete net 10.0.0.0: gateway tun0 fib 0: not in table\nroute: writing to routing socket: No such process\n",
            "delete net 10.0.0.0: gateway tun0: not in table\nroute: writing to routing socket: not in table\n",
        ] {
            assert!(
                ignore_removed_shell_cmd_not_found(Err(Error::ShellCommandError(
                    output.to_owned()
                )))
                .is_ok()
            );
        }
    }

    #[test]
    fn removed_shell_cmd_preserves_other_errors() {
        for output in [
            "ifconfig: ioctl (SIOCDIFADDR): Operation not permitted\n",
            "route: writing to routing socket: Permission denied\n",
            "route: route has not been found: Permission denied\n",
            "route: route has not been found: No such process\n",
            "delete net 10.0.0.0: gateway tun0: not in table\nroute: Permission denied\n",
            "delete net 10.0.0.0: gateway tun0: not in table\nroute: writing to routing socket: No such process\nroute: Permission denied\n",
            "unexpected command: not in table\n",
            "",
        ] {
            assert!(matches!(
                ignore_removed_shell_cmd_not_found(Err(Error::ShellCommandError(output.to_owned()))),
                Err(Error::ShellCommandError(actual)) if actual == output
            ));
        }

        assert!(matches!(
            ignore_removed_shell_cmd_not_found(Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            )
            .into())),
            Err(Error::IOError(_))
        ));
    }
}
