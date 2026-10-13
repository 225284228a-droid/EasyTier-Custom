//! Destination policies and ordered exit addresses for the policy VPN.

pub mod routes;

use cidr::{Ipv4Cidr, Ipv6Cidr};
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VpnRuleAction {
    /// Use the first reachable exit; drop while all exits are unavailable.
    #[default]
    Exit,
    Direct,
    Block,
}

impl VpnRuleAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exit => "exit",
            Self::Direct => "direct",
            Self::Block => "block",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpnCidr {
    V4(Ipv4Cidr),
    V6(Ipv6Cidr),
}

impl PartialOrd for VpnCidr {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for VpnCidr {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.first_address()
            .cmp(&other.first_address())
            .then(self.prefix_len().cmp(&other.prefix_len()))
    }
}

impl<'de> Deserialize<'de> for VpnCidr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
impl Serialize for VpnCidr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl std::fmt::Display for VpnCidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::V4(c) => write!(f, "{c}"),
            Self::V6(c) => write!(f, "{c}"),
        }
    }
}
impl FromStr for VpnCidr {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(c) = Ipv4Cidr::from_str(s) {
            return Ok(Self::V4(c));
        }
        if let Ok(c) = Ipv6Cidr::from_str(s) {
            return Ok(Self::V6(c));
        }
        anyhow::bail!("invalid VPN rule CIDR: {s:?}")
    }
}
impl VpnCidr {
    pub fn is_ipv4(self) -> bool {
        matches!(self, Self::V4(_))
    }
    pub fn is_default_route(self) -> bool {
        self.prefix_len() == 0
    }
    pub fn prefix_len(self) -> u8 {
        match self {
            Self::V4(c) => c.network_length(),
            Self::V6(c) => c.network_length(),
        }
    }
    pub fn first_address(self) -> IpAddr {
        match self {
            Self::V4(c) => c.first_address().into(),
            Self::V6(c) => c.first_address().into(),
        }
    }
    pub fn contains(self, address: &IpAddr) -> bool {
        match (self, address) {
            (Self::V4(c), IpAddr::V4(a)) => c.contains(a),
            (Self::V6(c), IpAddr::V6(a)) => c.contains(a),
            _ => false,
        }
    }
    pub fn host(address: IpAddr) -> Self {
        match address {
            IpAddr::V4(a) => Self::V4(Ipv4Cidr::new(a, 32).unwrap()),
            IpAddr::V6(a) => Self::V6(Ipv6Cidr::new(a, 128).unwrap()),
        }
    }
    pub fn split(self) -> Option<(Self, Self)> {
        let p = self.prefix_len();
        match self {
            Self::V4(c) if p < 32 => {
                let a = u32::from(c.first_address());
                Some((
                    Self::V4(Ipv4Cidr::new(a.into(), p + 1).unwrap()),
                    Self::V4(Ipv4Cidr::new((a | (1 << (31 - p))).into(), p + 1).unwrap()),
                ))
            }
            Self::V6(c) if p < 128 => {
                let a = u128::from(c.first_address());
                Some((
                    Self::V6(Ipv6Cidr::new(a.into(), p + 1).unwrap()),
                    Self::V6(Ipv6Cidr::new((a | (1 << (127 - p))).into(), p + 1).unwrap()),
                ))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VpnRuleConfig {
    pub cidr: VpnCidr,
    #[serde(default)]
    pub action: VpnRuleAction,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exits: Vec<IpAddr>,
    #[serde(default)]
    pub priority: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VpnConfig {
    #[serde(default)]
    pub exit_enabled: bool,
    #[serde(default = "default_true")]
    pub dns_protect: bool,
    #[serde(default = "default_true")]
    pub ipv6_protect: bool,
    #[serde(default = "default_true")]
    pub ignore_private: bool,
    #[serde(default)]
    pub rules: Vec<VpnRuleConfig>,
}
fn default_true() -> bool {
    true
}
impl Default for VpnConfig {
    fn default() -> Self {
        Self {
            exit_enabled: false,
            dns_protect: true,
            ipv6_protect: true,
            ignore_private: true,
            rules: vec![],
        }
    }
}
impl VpnConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        for rule in &self.rules {
            if rule.action == VpnRuleAction::Exit && rule.exits.is_empty() {
                anyhow::bail!("vpn rule {} requires at least one exit address", rule.cidr);
            }
            let mut seen = std::collections::BTreeSet::new();
            for address in &rule.exits {
                if address.is_unspecified() || address.is_multicast() || address.is_loopback() {
                    anyhow::bail!("vpn rule {} has invalid exit address {address}", rule.cidr);
                }
                if !seen.insert(address) {
                    anyhow::bail!(
                        "vpn rule {} has duplicate exit address {address}",
                        rule.cidr
                    );
                }
            }
        }
        Ok(())
    }
    pub fn client_active(&self) -> bool {
        !self.rules.is_empty()
    }
    pub fn exit_addresses(&self) -> std::collections::BTreeSet<IpAddr> {
        self.rules
            .iter()
            .filter(|r| r.action == VpnRuleAction::Exit)
            .flat_map(|r| r.exits.iter().copied())
            .collect()
    }
    pub fn match_rule_index(&self, address: &IpAddr) -> Option<usize> {
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, r)| r.cidr.contains(address))
            .max_by(|(a, ra), (b, rb)| {
                ra.cidr
                    .prefix_len()
                    .cmp(&rb.cidr.prefix_len())
                    .then(rb.priority.cmp(&ra.priority))
                    .then(b.cmp(a))
            })
            .map(|(i, _)| i)
    }
    fn has_global_exit(&self, ipv4: bool) -> bool {
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, r)| r.cidr.is_default_route() && r.cidr.is_ipv4() == ipv4)
            .min_by_key(|(i, r)| (r.priority, *i))
            .is_some_and(|(_, r)| r.action == VpnRuleAction::Exit)
    }
    pub fn has_global_v4_exit(&self) -> bool {
        self.has_global_exit(true)
    }
    pub fn has_global_v6_exit(&self) -> bool {
        self.has_global_exit(false)
    }
    pub fn has_v4_exit_rule(&self) -> bool {
        self.rules
            .iter()
            .any(|r| r.action == VpnRuleAction::Exit && r.cidr.is_ipv4())
    }
    pub fn has_v6_exit_rule(&self) -> bool {
        self.rules
            .iter()
            .any(|r| r.action == VpnRuleAction::Exit && !r.cidr.is_ipv4())
    }
}

#[cfg(any(feature = "web-client", feature = "browser-config"))]
pub mod pb {
    use super::*;
    #[derive(Debug)]
    pub struct ParseVpnError(pub String);
    impl std::fmt::Display for ParseVpnError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "invalid vpn config: {}", self.0)
        }
    }
    impl std::error::Error for ParseVpnError {}
    pub fn from_pb(
        vpn: easytier_proto::api::manage::VpnConfig,
    ) -> Result<VpnConfig, ParseVpnError> {
        let mut rules = Vec::with_capacity(vpn.rules.len());
        for rule in vpn.rules {
            let cidr = rule
                .cidr
                .parse()
                .map_err(|e| ParseVpnError(format!("rule {:?}: {e}", rule.cidr)))?;
            let action = match rule.action.as_str() {
                "" | "exit" => VpnRuleAction::Exit,
                "direct" => VpnRuleAction::Direct,
                "block" => VpnRuleAction::Block,
                other => {
                    return Err(ParseVpnError(format!(
                        "rule {cidr}: invalid action {other:?}"
                    )));
                }
            };
            let exits = rule
                .exits
                .into_iter()
                .map(|a| {
                    a.parse()
                        .map_err(|e| ParseVpnError(format!("rule {cidr}: invalid exit {a:?}: {e}")))
                })
                .collect::<Result<Vec<IpAddr>, _>>()?;
            rules.push(VpnRuleConfig {
                cidr,
                action,
                exits,
                priority: rule.priority.unwrap_or_default(),
            });
        }
        let config = VpnConfig {
            exit_enabled: vpn.exit_enabled,
            dns_protect: vpn.dns_protect.unwrap_or(true),
            ipv6_protect: vpn.ipv6_protect.unwrap_or(true),
            ignore_private: vpn.ignore_private.unwrap_or(true),
            rules,
        };
        config
            .validate()
            .map_err(|e| ParseVpnError(e.to_string()))?;
        Ok(config)
    }
    pub fn to_pb(vpn: &VpnConfig) -> easytier_proto::api::manage::VpnConfig {
        easytier_proto::api::manage::VpnConfig {
            exit_enabled: vpn.exit_enabled,
            dns_protect: Some(vpn.dns_protect),
            ipv6_protect: Some(vpn.ipv6_protect),
            ignore_private: Some(vpn.ignore_private),
            rules: vpn
                .rules
                .iter()
                .map(|r| easytier_proto::api::manage::VpnRuleConfig {
                    cidr: r.cidr.to_string(),
                    action: r.action.as_str().to_owned(),
                    exits: r.exits.iter().map(ToString::to_string).collect(),
                    priority: Some(r.priority),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_automatic_activation() {
        let mut c: VpnConfig = toml::from_str("").unwrap();
        assert!(!c.client_active());
        assert!(!c.exit_enabled);
        assert!(c.dns_protect && c.ipv6_protect && c.ignore_private);
        c.rules.push(VpnRuleConfig {
            cidr: "0.0.0.0/0".parse().unwrap(),
            action: VpnRuleAction::Exit,
            exits: vec!["10.0.0.2".parse().unwrap(), "fd00::3".parse().unwrap()],
            priority: 0,
        });
        assert!(c.client_active());
        c.validate().unwrap();
        let parsed: VpnConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(parsed, c);
    }
    #[test]
    fn requires_valid_unique_addresses_for_exit_rules() {
        for exits in [vec![], vec!["0.0.0.0"], vec!["10.0.0.2", "10.0.0.2"]] {
            let c = VpnConfig {
                rules: vec![VpnRuleConfig {
                    cidr: "::/0".parse().unwrap(),
                    action: VpnRuleAction::Exit,
                    exits: exits.into_iter().map(|a| a.parse().unwrap()).collect(),
                    priority: 0,
                }],
                ..Default::default()
            };
            assert!(c.validate().is_err());
        }
    }
    #[test]
    fn narrower_match_and_duplicate_priority_are_deterministic() {
        let mut c = VpnConfig::default();
        for (cidr, priority) in [("0.0.0.0/0", 0), ("8.0.0.0/8", 10), ("8.0.0.0/8", 0)] {
            c.rules.push(VpnRuleConfig {
                cidr: cidr.parse().unwrap(),
                action: VpnRuleAction::Block,
                exits: vec![],
                priority,
            });
        }
        assert_eq!(c.match_rule_index(&"8.8.8.8".parse().unwrap()), Some(2));
    }
    #[test]
    fn removed_schema_fields_are_rejected() {
        for field in ["enabled = true", "fail_mode = 'direct'", "exits = []"] {
            assert!(toml::from_str::<VpnConfig>(field).is_err());
        }
        assert!(
            toml::from_str::<VpnConfig>("[[rules]]\ncidr = '0.0.0.0/0'\nexit = 'old-name'")
                .is_err()
        );
    }
    #[cfg(any(feature = "web-client", feature = "browser-config"))]
    #[test]
    fn protobuf_omission_and_explicit_false_preserve_protection_defaults() {
        let c = pb::from_pb(easytier_proto::api::manage::VpnConfig::default()).unwrap();
        assert_eq!(c, VpnConfig::default());
        let c = pb::from_pb(easytier_proto::api::manage::VpnConfig {
            dns_protect: Some(false),
            ipv6_protect: Some(false),
            ignore_private: Some(false),
            ..Default::default()
        })
        .unwrap();
        assert!(!c.dns_protect && !c.ipv6_protect && !c.ignore_private);
        assert_eq!(pb::from_pb(pb::to_pb(&c)).unwrap(), c);
    }
}
