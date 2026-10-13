//! Shared CIDR partitioning for packet policy and native/Android capture.

use super::{VpnCidr, VpnConfig, VpnRuleAction};
use std::net::IpAddr;

/// Overlay regions retain their normal EasyTier path. Physical regions are
/// local advertised subnets and must never be recaptured into this instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VpnRouteExclusions {
    pub overlay: Vec<VpnCidr>,
    pub physical: Vec<VpnCidr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpnRouteRegion {
    pub cidr: VpnCidr,
    pub action: VpnRuleAction,
    /// Original user rule used for exit selection and traffic counters.
    pub rule_index: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VpnRoutePlan {
    pub regions: Vec<VpnRouteRegion>,
    /// Full positive TUN set: existing overlay plus policy capture.
    pub capture_routes: Vec<VpnCidr>,
}

pub const PRIVATE_CIDRS: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "fc00::/7",
    "::1/128",
    "fe80::/10",
];

pub fn private_cidrs() -> Vec<VpnCidr> {
    PRIVATE_CIDRS.iter().map(|c| c.parse().unwrap()).collect()
}

pub fn is_private_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(a) => {
            a.is_private()
                || a.is_loopback()
                || a.is_link_local()
                || (u32::from(*a) & 0xffc0_0000) == 0x6440_0000
        }
        IpAddr::V6(a) => a.is_unique_local() || a.is_loopback() || a.is_unicast_link_local(),
    }
}

/// CIDR subtraction produces disjoint positive routes without relying on
/// platform metrics, synthetic /1 priority, or negative Android routes.
pub fn subtract_cidrs(cidr: VpnCidr, excluded: &[VpnCidr]) -> Vec<VpnCidr> {
    let relevant: Vec<_> = excluded
        .iter()
        .copied()
        .filter(|c| {
            c.is_ipv4() == cidr.is_ipv4()
                && (c.contains(&cidr.first_address()) || cidr.contains(&c.first_address()))
        })
        .collect();
    if relevant.iter().any(|c| c.prefix_len() <= cidr.prefix_len()) {
        return vec![];
    }
    if relevant.is_empty() {
        return vec![cidr];
    }
    let Some((a, b)) = cidr.split() else {
        return vec![];
    };
    let mut result = subtract_cidrs(a, &relevant);
    result.extend(subtract_cidrs(b, &relevant));
    result
}

/// Normalize/merge a CIDR union. Removing covered regions keeps Android's
/// route count bounded when mesh routes overlap policy capture.
pub fn normalize_cidrs(cidrs: impl IntoIterator<Item = VpnCidr>) -> Vec<VpnCidr> {
    let mut sorted: Vec<_> = cidrs.into_iter().collect();
    sorted.sort_by_key(|c| (c.is_ipv4(), c.prefix_len(), c.first_address()));
    let mut result: Vec<VpnCidr> = vec![];
    for c in sorted {
        if !result
            .iter()
            .any(|r| r.contains(&c.first_address()) && r.prefix_len() <= c.prefix_len())
        {
            result.push(c);
        }
    }
    loop {
        let mut merged = None;
        for (i, a) in result.iter().enumerate() {
            if a.prefix_len() == 0 {
                continue;
            }
            let parent = match *a {
                VpnCidr::V4(c) => VpnCidr::V4(
                    cidr::Ipv4Cidr::new(
                        (u32::from(c.first_address())
                            & u32::MAX
                                .checked_shl(u32::from(33 - c.network_length()))
                                .unwrap_or(0))
                        .into(),
                        c.network_length() - 1,
                    )
                    .unwrap(),
                ),
                VpnCidr::V6(c) => VpnCidr::V6(
                    cidr::Ipv6Cidr::new(
                        (u128::from(c.first_address())
                            & u128::MAX
                                .checked_shl(u32::from(129 - c.network_length()))
                                .unwrap_or(0))
                        .into(),
                        c.network_length() - 1,
                    )
                    .unwrap(),
                ),
            };
            if let Some(j) = result.iter().enumerate().find_map(|(j, b)| {
                (j != i
                    && a.prefix_len() == b.prefix_len()
                    && a.is_ipv4() == b.is_ipv4()
                    && parent.contains(&b.first_address()))
                .then_some(j)
            }) {
                merged = Some((i, j, parent));
                break;
            }
        }
        let Some((i, j, parent)) = merged else {
            break;
        };
        result.remove(i.max(j));
        result.remove(i.min(j));
        result.push(parent);
    }
    result.sort_by_key(|c| (c.is_ipv4(), c.first_address(), c.prefix_len()));
    result
}

pub fn compile_vpn_routes(
    config: &VpnConfig,
    exclusions: &VpnRouteExclusions,
    dns_servers: &[IpAddr],
) -> VpnRoutePlan {
    // Overlay always wins over ordinary private-address bypass. Local
    // advertised subnets remain physical even if a remote advertisement overlaps.
    let overlay: Vec<_> = exclusions
        .overlay
        .iter()
        .flat_map(|c| subtract_cidrs(*c, &exclusions.physical))
        .collect();
    let mut forbidden = exclusions.physical.clone();
    forbidden.extend(overlay.iter().copied());
    if config.ignore_private {
        forbidden.extend(private_cidrs());
    }
    let mut claimed = normalize_cidrs(forbidden.clone());
    let mut regions = vec![];
    let mut indices: Vec<_> = (0..config.rules.len()).collect();
    indices.sort_by_key(|&i| {
        (
            std::cmp::Reverse(config.rules[i].cidr.prefix_len()),
            config.rules[i].priority,
            i,
        )
    });
    for i in indices {
        let rule = &config.rules[i];
        regions.extend(subtract_cidrs(rule.cidr, &claimed).into_iter().map(|cidr| {
            VpnRouteRegion {
                cidr,
                action: rule.action,
                rule_index: Some(i),
            }
        }));
        claimed.push(rule.cidr);
    }
    let has_exit = regions.iter().any(|r| r.action == VpnRuleAction::Exit);
    let has_v4_exit = regions
        .iter()
        .any(|r| r.action == VpnRuleAction::Exit && r.cidr.is_ipv4());
    if config.ipv6_protect && has_v4_exit {
        regions.extend(
            subtract_cidrs("::/0".parse().unwrap(), &claimed)
                .into_iter()
                .map(|cidr| VpnRouteRegion {
                    cidr,
                    action: VpnRuleAction::Block,
                    rule_index: None,
                }),
        );
    }
    if config.dns_protect && has_exit {
        for address in dns_servers
            .iter()
            .filter(|a| !a.is_unspecified() && !a.is_multicast())
        {
            if forbidden.iter().any(|c| c.contains(address)) {
                continue;
            }
            let selected = config.match_rule_index(address);
            let action = selected
                .map(|i| config.rules[i].action)
                .unwrap_or(VpnRuleAction::Block);
            if action == VpnRuleAction::Direct {
                continue;
            }
            // A public resolver pin agrees with the matched rule. It may not
            // override an explicit direct rule or any hard exclusion.
            let host = VpnCidr::host(*address);
            regions = regions
                .into_iter()
                .flat_map(|r| {
                    subtract_cidrs(r.cidr, &[host])
                        .into_iter()
                        .map(move |cidr| VpnRouteRegion { cidr, ..r.clone() })
                })
                .collect();
            regions.push(VpnRouteRegion {
                cidr: host,
                action,
                rule_index: selected,
            });
        }
    }
    let capture_routes = normalize_cidrs(
        overlay.into_iter().chain(
            regions
                .iter()
                .filter(|r| r.action != VpnRuleAction::Direct)
                .map(|r| r.cidr),
        ),
    );
    VpnRoutePlan {
        regions,
        capture_routes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::vpn::VpnRuleConfig;
    fn rule(cidr: &str, action: VpnRuleAction) -> VpnRuleConfig {
        VpnRuleConfig {
            cidr: cidr.parse().unwrap(),
            action,
            exits: vec!["10.0.0.2".parse().unwrap()],
            priority: 0,
        }
    }
    fn captured(plan: &VpnRoutePlan, address: &str) -> bool {
        plan.capture_routes
            .iter()
            .any(|c| c.contains(&address.parse().unwrap()))
    }
    #[test]
    fn default_and_explicit_half_have_no_metric_conflict() {
        let config = VpnConfig {
            ignore_private: false,
            rules: vec![
                rule("0.0.0.0/0", VpnRuleAction::Exit),
                rule("0.0.0.0/1", VpnRuleAction::Direct),
            ],
            ..Default::default()
        };
        let p = compile_vpn_routes(&config, &Default::default(), &[]);
        assert!(!captured(&p, "8.8.8.8"));
        assert!(captured(&p, "203.0.113.1"));
    }
    #[test]
    fn private_bypass_overrides_even_host_block_but_overlay_still_routes() {
        let config = VpnConfig {
            rules: vec![
                rule("0.0.0.0/0", VpnRuleAction::Exit),
                rule("192.168.1.8/32", VpnRuleAction::Block),
            ],
            ..Default::default()
        };
        let x = VpnRouteExclusions {
            overlay: vec![
                "10.144.0.0/16".parse().unwrap(),
                "203.0.113.0/24".parse().unwrap(),
            ],
            physical: vec!["192.168.2.0/24".parse().unwrap()],
        };
        let p = compile_vpn_routes(&config, &x, &[]);
        assert!(!captured(&p, "192.168.1.8"));
        assert!(captured(&p, "10.144.0.2"));
        assert!(
            !p.regions
                .iter()
                .any(|r| r.cidr.contains(&"203.0.113.8".parse().unwrap()))
        );
        assert!(captured(&p, "203.0.113.8"));
        assert!(!captured(&p, "192.168.2.8"));
    }
    #[test]
    fn overlap_partition_matches_packet_rule_at_boundaries() {
        let c = VpnConfig {
            ignore_private: false,
            rules: vec![
                rule("0.0.0.0/0", VpnRuleAction::Exit),
                rule("8.8.0.0/16", VpnRuleAction::Block),
                rule("8.8.8.0/24", VpnRuleAction::Direct),
            ],
            ..Default::default()
        };
        let p = compile_vpn_routes(&c, &Default::default(), &[]);
        for address in [
            "8.7.255.255",
            "8.8.0.0",
            "8.8.7.255",
            "8.8.8.0",
            "8.8.8.255",
            "8.8.9.0",
            "8.9.0.0",
        ] {
            let a = address.parse().unwrap();
            let matching: Vec<_> = p.regions.iter().filter(|r| r.cidr.contains(&a)).collect();
            assert_eq!(matching.len(), 1);
            assert_eq!(matching[0].rule_index, c.match_rule_index(&a));
        }
    }
    #[test]
    fn ipv6_guard_and_dns_respect_direct_and_private_exemptions() {
        let c = VpnConfig {
            rules: vec![
                rule("0.0.0.0/0", VpnRuleAction::Exit),
                rule("2001:db8::/32", VpnRuleAction::Direct),
            ],
            ..Default::default()
        };
        let p = compile_vpn_routes(
            &c,
            &Default::default(),
            &[
                "192.168.1.1".parse().unwrap(),
                "2001:db8::53".parse().unwrap(),
                "2606:4700:4700::1111".parse().unwrap(),
            ],
        );
        assert!(!captured(&p, "192.168.1.1"));
        assert!(!captured(&p, "2001:db8::53"));
        assert!(captured(&p, "2606:4700:4700::1111"));
        assert!(p.regions.iter().any(
            |r| r.cidr.contains(&"2606:4700:4700::1111".parse().unwrap())
                && r.action == VpnRuleAction::Block
        ));
    }
    #[test]
    fn inactive_policy_keeps_only_overlay_capture() {
        let p = compile_vpn_routes(
            &VpnConfig::default(),
            &VpnRouteExclusions {
                overlay: vec!["10.0.0.0/24".parse().unwrap()],
                physical: vec![],
            },
            &[],
        );
        assert!(p.regions.is_empty());
        assert!(captured(&p, "10.0.0.2"));
        assert!(!captured(&p, "8.8.8.8"));
    }
    #[test]
    fn narrower_exit_still_guards_public_dns_and_uncovered_ipv6() {
        let c = VpnConfig {
            rules: vec![
                rule("203.0.113.0/24", VpnRuleAction::Exit),
                rule("9.9.9.9/32", VpnRuleAction::Direct),
                rule("2001:db8::/32", VpnRuleAction::Direct),
            ],
            ..Default::default()
        };
        let p = compile_vpn_routes(
            &c,
            &Default::default(),
            &[
                "8.8.8.8".parse().unwrap(),
                "9.9.9.9".parse().unwrap(),
                "192.168.1.1".parse().unwrap(),
            ],
        );
        for addr in ["8.8.8.8", "2606:4700:4700::1111", "64:ff9b::808:808"] {
            assert!(captured(&p, addr));
            assert!(p.regions.iter().any(
                |r| r.cidr.contains(&addr.parse().unwrap()) && r.action == VpnRuleAction::Block
            ));
        }
        for addr in ["9.9.9.9", "192.168.1.1", "2001:db8::53", "fd00::1"] {
            assert!(!captured(&p, addr));
        }
    }
    #[test]
    fn entirely_shadowed_or_exempt_exit_does_not_activate_guards() {
        for rules in [
            vec![
                rule("203.0.113.0/24", VpnRuleAction::Direct),
                rule("203.0.113.0/24", VpnRuleAction::Exit),
            ],
            vec![rule("192.168.1.0/24", VpnRuleAction::Exit)],
        ] {
            let c = VpnConfig {
                rules,
                ..Default::default()
            };
            let p = compile_vpn_routes(&c, &Default::default(), &["8.8.8.8".parse().unwrap()]);
            assert!(!captured(&p, "8.8.8.8"));
            assert!(!captured(&p, "2606:4700:4700::1111"));
        }
    }
}
