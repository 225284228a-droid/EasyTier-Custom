//! Shared destination routes for desktop and Android capture.
use super::{VpnCidr, VpnConfig};

/// Preserve mesh/proxy routes and local advertised subnet ownership.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VpnRouteExclusions {
    pub overlay: Vec<VpnCidr>,
    pub physical: Vec<VpnCidr>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VpnRoutePlan {
    pub exit_routes: Vec<VpnCidr>,
    /// Full positive TUN set: existing overlay plus VPN capture.
    pub capture_routes: Vec<VpnCidr>,
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

pub fn compile_vpn_routes(config: &VpnConfig, exclusions: &VpnRouteExclusions) -> VpnRoutePlan {
    let overlay = normalize_cidrs(
        exclusions
            .overlay
            .iter()
            .flat_map(|c| subtract_cidrs(*c, &exclusions.physical)),
    );
    let excluded = normalize_cidrs(exclusions.physical.iter().chain(&overlay).copied());
    let exit_routes = normalize_cidrs(
        config
            .rules
            .iter()
            .flat_map(|r| subtract_cidrs(r.cidr, &excluded)),
    );
    let capture_routes = normalize_cidrs(overlay.into_iter().chain(exit_routes.iter().copied()));
    VpnRoutePlan {
        exit_routes,
        capture_routes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::vpn::VpnRuleConfig;
    fn config(cidrs: &[&str]) -> VpnConfig {
        VpnConfig {
            rules: cidrs
                .iter()
                .map(|c| VpnRuleConfig {
                    cidr: c.parse().unwrap(),
                    exit: Some("10.0.0.2".parse().unwrap()),
                })
                .collect(),
        }
    }
    fn contains(routes: &[VpnCidr], address: &str) -> bool {
        routes.iter().any(|c| c.contains(&address.parse().unwrap()))
    }
    #[test]
    fn default_routes_preserve_overlay_and_physical_ownership() {
        let exclusions = VpnRouteExclusions {
            overlay: vec![
                "10.66.0.0/24".parse().unwrap(),
                "fd42::/64".parse().unwrap(),
            ],
            physical: vec!["192.168.1.0/24".parse().unwrap()],
        };
        let plan = compile_vpn_routes(&config(&["0.0.0.0/0", "::/0"]), &exclusions);
        for ip in ["10.66.0.2", "fd42::2"] {
            assert!(!contains(&plan.exit_routes, ip));
            assert!(contains(&plan.capture_routes, ip));
        }
        assert!(!contains(&plan.capture_routes, "192.168.1.2"));
        for ip in ["8.8.8.8", "172.20.0.1", "2001:db8::1", "fd99::1"] {
            assert!(contains(&plan.exit_routes, ip));
        }
    }
    #[test]
    fn only_explicit_subnets_are_captured_and_union_is_compact() {
        let plan = compile_vpn_routes(
            &config(&["203.0.113.0/24", "203.0.113.0/25", "203.0.113.128/25"]),
            &Default::default(),
        );
        assert_eq!(plan.exit_routes, vec!["203.0.113.0/24".parse().unwrap()]);
        assert!(!contains(&plan.capture_routes, "8.8.8.8"));
        assert!(!contains(&plan.capture_routes, "2606:4700:4700::1111"));
    }
    #[test]
    fn empty_rules_keep_android_mesh_routes_only() {
        let exclusions = VpnRouteExclusions {
            overlay: vec!["10.66.0.0/24".parse().unwrap()],
            ..Default::default()
        };
        let plan = compile_vpn_routes(&VpnConfig::default(), &exclusions);
        assert!(plan.exit_routes.is_empty());
        assert_eq!(plan.capture_routes, exclusions.overlay);
    }
    #[test]
    fn subtract_and_merge_preserve_both_address_families() {
        for (parent, a, b) in [
            ("0.0.0.0/0", "0.0.0.0/1", "128.0.0.0/1"),
            ("::/0", "::/1", "8000::/1"),
        ] {
            let parent = parent.parse::<VpnCidr>().unwrap();
            let a = a.parse::<VpnCidr>().unwrap();
            let b = b.parse::<VpnCidr>().unwrap();
            assert_eq!(subtract_cidrs(parent, &[a]), vec![b]);
            assert_eq!(normalize_cidrs([a, b]), vec![parent]);
        }
    }
}
