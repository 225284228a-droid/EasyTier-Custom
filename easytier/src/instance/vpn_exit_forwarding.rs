//! Exit-side platform setup for the multi-exit VPN.
//!
//! IPv4 exit traffic is always NATed by the userspace proxy engines. IPv6
//! exit traffic is handled per platform:
//!
//! * Linux: kernel forwarding plus an ip6tables MASQUERADE rule;
//! * macOS: kernel forwarding plus a pf anchor (`com.easytier.vpn66`);
//! * Windows: no kernel NAT66 — TCP and UDP are relayed by the userspace
//!   proxy engines instead, so nothing is configured here.

/// Enables or disables exit forwarding for the given TUN interface.
///
/// `v6_prefix` is the network's IPv6 prefix; `None` means the network has no
/// IPv6 addressing, in which case kernel platforms have nothing to install.
pub async fn sync_vpn_exit_forwarding(enabled: bool, tun_ifname: &str, v6_prefix: Option<String>) {
    #[cfg(target_os = "linux")]
    {
        sync_linux(enabled, tun_ifname, v6_prefix).await;
    }
    #[cfg(target_os = "macos")]
    {
        sync_macos(enabled, tun_ifname, v6_prefix).await;
    }
    #[cfg(target_os = "windows")]
    {
        // Kernel forwarding stays off; the userspace proxy engines relay v6
        // exit TCP and UDP, and ICMPv6 exit packets are dropped by design.
        let _ = (enabled, tun_ifname, v6_prefix);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        if enabled && v6_prefix.is_some() {
            tracing::warn!(
                "ipv6 exit forwarding has no platform support here; \
                 ipv4 exit traffic still works"
            );
        }
        let _ = (enabled, tun_ifname);
    }
}

#[cfg(target_os = "linux")]
async fn sync_linux(enabled: bool, tun_ifname: &str, v6_prefix: Option<String>) {
    use std::process::Command;

    fn run(cmd: &mut Command) -> bool {
        match cmd.output() {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                tracing::warn!(
                    cmd = ?cmd,
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "vpn exit platform command failed"
                );
                false
            }
            Err(err) => {
                tracing::warn!(?cmd, ?err, "vpn exit platform command failed to start");
                false
            }
        }
    }

    if enabled {
        for path in [
            "/proc/sys/net/ipv6/conf/all/forwarding",
            "/proc/sys/net/ipv6/conf/default/forwarding",
        ] {
            if let Err(err) = std::fs::write(path, b"1") {
                tracing::warn!(path, ?err, "failed to enable ipv6 forwarding");
            }
        }

        if let Some(prefix) = v6_prefix {
            // MASQUERADE every forwarded packet sourced from the mesh prefix
            // that leaves through any interface except the TUN. Rule is
            // idempotent: -C checks, -A adds only when missing.
            let check = Command::new("ip6tables")
                .args([
                    "-t",
                    "nat",
                    "-C",
                    "POSTROUTING",
                    "!",
                    "-o",
                    tun_ifname,
                    "-s",
                    &prefix,
                    "-j",
                    "MASQUERADE",
                ])
                .output();
            let present = matches!(&check, Ok(output) if output.status.success());
            if !present {
                run(Command::new("ip6tables").args([
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "!",
                    "-o",
                    tun_ifname,
                    "-s",
                    &prefix,
                    "-j",
                    "MASQUERADE",
                ]));
            }
        }
    } else {
        if let Some(prefix) = v6_prefix {
            // Delete our rule if present; failure just means it was absent.
            let _ = Command::new("ip6tables")
                .args([
                    "-t",
                    "nat",
                    "-D",
                    "POSTROUTING",
                    "!",
                    "-o",
                    tun_ifname,
                    "-s",
                    &prefix,
                    "-j",
                    "MASQUERADE",
                ])
                .output();
        }
    }
}

#[cfg(target_os = "macos")]
async fn sync_macos(enabled: bool, tun_ifname: &str, v6_prefix: Option<String>) {
    use crate::common::ifcfg::{IfConfiger, IfConfiguerTrait as _};

    let ifcfg = IfConfiger {};
    if let Err(err) = ifcfg
        .sync_ipv6_exit_forwarding(tun_ifname, v6_prefix.as_deref(), enabled)
        .await
    {
        tracing::warn!(
            ?err,
            enabled,
            "failed to sync ipv6 exit forwarding via pf; \
             ipv6 exit traffic may not work (ipv4 is unaffected)"
        );
    }
}

/// Formats the network prefix used for the masquerade rule.
pub fn v6_masquerade_prefix(prefix: &cidr::Ipv6Inet) -> String {
    prefix.to_string()
}
