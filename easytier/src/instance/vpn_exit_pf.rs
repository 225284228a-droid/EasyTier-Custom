//! pf anchor rule construction for macOS IPv6 exit forwarding (NAT66).
//!
//! The rule text and pf.conf editing live here, platform-independent, so the
//! string handling is unit-testable on every platform; `ifcfg::darwin` only
//! executes the resulting commands.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// pf anchor owning EasyTier's NAT66 rules.
pub(crate) const IPV6_EXIT_ANCHOR_NAME: &str = "com.easytier.vpn66";
pub(crate) const IPV6_EXIT_PF_CONF: &str = "/etc/pf.conf";
/// Marks the line EasyTier added to pf.conf; removal only deletes lines
/// carrying this exact marker.
pub(crate) const IPV6_EXIT_ANCHOR_MARKER: &str = "# added by EasyTier for ipv6 exit";

/// NAT66 rule: masquerade mesh traffic leaving through any interface except
/// the TUN, out of the physical default-route interface.
pub(crate) fn ipv6_exit_pf_rule(tun_ifname: &str, mesh_prefix: &str, ext_ifname: &str) -> String {
    format!("nat on ! ({tun_ifname}) inet6 from {mesh_prefix} to any -> ({ext_ifname})")
}

pub(crate) fn ipv6_exit_anchor_directive() -> String {
    format!("nat-anchor \"{IPV6_EXIT_ANCHOR_NAME}\" {IPV6_EXIT_ANCHOR_MARKER}")
}

/// Returns the pf.conf content with the anchor directive inserted, or `None`
/// when it is already present. The directive goes after the last existing
/// `nat-anchor` line so pf evaluates the anchors together.
pub(crate) fn ensure_ipv6_exit_anchor_directive(conf: &str) -> Option<String> {
    if conf
        .lines()
        .any(|line| line.contains(IPV6_EXIT_ANCHOR_MARKER))
    {
        return None;
    }
    let directive = ipv6_exit_anchor_directive();
    let mut last_nat_anchor = None;
    for (index, line) in conf.lines().enumerate() {
        if line.trim_start().starts_with("nat-anchor") {
            last_nat_anchor = Some(index);
        }
    }
    let mut lines = conf.lines().map(ToString::to_string).collect::<Vec<_>>();
    match last_nat_anchor {
        Some(index) => lines.insert(index + 1, directive),
        None => lines.push(directive),
    }
    Some(lines.join("\n") + if conf.ends_with('\n') { "\n" } else { "" })
}

/// Returns the pf.conf content with every EasyTier anchor directive removed.
pub(crate) fn remove_ipv6_exit_anchor_directive(conf: &str) -> String {
    let kept = conf
        .lines()
        .filter(|line| !line.contains(IPV6_EXIT_ANCHOR_MARKER))
        .collect::<Vec<_>>();
    if kept.len() == conf.lines().count() {
        return conf.to_string();
    }
    kept.join("\n") + if conf.ends_with('\n') { "\n" } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pf_rule_targets_non_tun_interfaces() {
        let rule = ipv6_exit_pf_rule("utun5", "fd00:abcd::/64", "en0");
        assert_eq!(
            rule,
            "nat on ! (utun5) inet6 from fd00:abcd::/64 to any -> (en0)"
        );
    }

    #[test]
    fn anchor_directive_insertion_is_idempotent() {
        let conf = "scrub-anchor \"com.apple/*\"\nnat-anchor \"com.apple/*\"\n";
        let updated = ensure_ipv6_exit_anchor_directive(conf).unwrap();
        assert!(updated.contains(&ipv6_exit_anchor_directive()));
        assert!(updated.starts_with("scrub-anchor"));
        // inserted right after the last nat-anchor line
        let lines: Vec<&str> = updated.lines().collect();
        assert_eq!(lines[2], ipv6_exit_anchor_directive());

        assert!(ensure_ipv6_exit_anchor_directive(&updated).is_none());
    }

    #[test]
    fn anchor_directive_without_existing_nat_anchor_is_appended() {
        let conf = "set skip on lo0\n";
        let updated = ensure_ipv6_exit_anchor_directive(conf).unwrap();
        let lines: Vec<&str> = updated.lines().collect();
        assert_eq!(lines[1], ipv6_exit_anchor_directive());
    }

    #[test]
    fn anchor_directive_removal_keeps_other_lines() {
        let conf = format!(
            "nat-anchor \"com.apple/*\"\n{}\nanchor \"com.apple/*\"\n",
            ipv6_exit_anchor_directive()
        );
        let updated = remove_ipv6_exit_anchor_directive(&conf);
        assert!(!updated.contains(IPV6_EXIT_ANCHOR_MARKER));
        assert_eq!(
            updated,
            "nat-anchor \"com.apple/*\"\nanchor \"com.apple/*\"\n"
        );
        // content without our marker round-trips unchanged
        assert_eq!(
            remove_ipv6_exit_anchor_directive("nat-anchor \"com.apple/*\"\n"),
            "nat-anchor \"com.apple/*\"\n"
        );
    }
}
