//! Exit-side platform setup for the multi-exit VPN.
//!
//! IPv4 exit traffic is always NATed by the userspace proxy engines. IPv6
//! exit traffic is handled per platform:
//!
//! * Linux: kernel forwarding plus an ip6tables MASQUERADE rule;
//! * other platforms: no kernel NAT66 — TCP and UDP are relayed by the
//!   userspace proxy engines instead, so nothing is configured here.

/// Enables or disables exit forwarding for the given TUN interface.
///
/// `v6_prefix` is the network's IPv6 prefix; `None` means the network has no
/// IPv6 addressing, in which case kernel platforms have nothing to install.
pub async fn sync_vpn_exit_forwarding(enabled: bool, tun_ifname: &str, v6_prefix: Option<String>) {
    #[cfg(target_os = "linux")]
    {
        sync_linux(enabled, tun_ifname, v6_prefix).await;
    }
    #[cfg(not(target_os = "linux"))]
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
async fn sync_linux(enabled: bool, tun_ifname: &str, v6_prefix: Option<String>) {    use std::process::Command;

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

