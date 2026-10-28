//! Destination subnets mapped to a single virtual exit address.

pub mod routes;

use cidr::{Ipv4Cidr, Ipv6Cidr};
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, str::FromStr};

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
    /// Without an exit the subnet is captured and routed through the official
    /// `exit_nodes` list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<IpAddr>,
}

/// Client rules only. Offering exit forwarding is the official
/// `flags.enable_exit_node`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "VpnConfigFile")]
pub struct VpnConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<VpnRuleConfig>,
}

/// Earlier releases stored `exit_enabled` in `[vpn]`. Reading it keeps those
/// files loadable; it no longer enables anything.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VpnConfigFile {
    #[serde(default)]
    exit_enabled: Option<bool>,
    #[serde(default)]
    rules: Vec<VpnRuleConfig>,
}

impl From<VpnConfigFile> for VpnConfig {
    fn from(file: VpnConfigFile) -> Self {
        if file.exit_enabled.is_some() {
            tracing::warn!(
                "[vpn] exit_enabled is ignored; set flags.enable_exit_node to offer exit forwarding"
            );
        }
        Self { rules: file.rules }
    }
}
impl VpnConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut cidrs = std::collections::BTreeSet::new();
        for rule in &self.rules {
            anyhow::ensure!(
                cidrs.insert(rule.cidr),
                "duplicate VPN rule CIDR: {}",
                rule.cidr
            );
            if let Some(exit) = rule.exit {
                anyhow::ensure!(
                    !exit.is_unspecified() && !exit.is_multicast() && !exit.is_loopback(),
                    "vpn rule {} has invalid exit address {}",
                    rule.cidr,
                    exit
                );
            }
        }
        Ok(())
    }
    pub fn client_active(&self) -> bool {
        !self.rules.is_empty()
    }
    pub fn exit_addresses(&self) -> std::collections::BTreeSet<IpAddr> {
        self.rules.iter().filter_map(|r| r.exit).collect()
    }
    /// Some rule leaves its subnet to the official `exit_nodes` list.
    pub fn follows_exit_nodes(&self) -> bool {
        self.rules.iter().any(|r| r.exit.is_none())
    }
    pub fn match_rule_index(&self, address: &IpAddr) -> Option<usize> {
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, r)| r.cidr.contains(address))
            .max_by_key(|(_, r)| r.cidr.prefix_len())
            .map(|(i, _)| i)
    }
}

#[cfg(any(feature = "web-client", feature = "browser-config"))]
pub mod pb {
    use super::*;
    pub fn from_pb(vpn: easytier_proto::api::manage::VpnConfig) -> anyhow::Result<VpnConfig> {
        let rules = vpn
            .rules
            .into_iter()
            .map(|r| {
                Ok(VpnRuleConfig {
                    cidr: r.cidr.parse()?,
                    exit: match r.exit_addr.trim() {
                        "" => None,
                        exit => Some(exit.parse()?),
                    },
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let config = VpnConfig { rules };
        config.validate()?;
        Ok(config)
    }
    pub fn to_pb(vpn: &VpnConfig) -> easytier_proto::api::manage::VpnConfig {
        easytier_proto::api::manage::VpnConfig {
            rules: vpn
                .rules
                .iter()
                .map(|r| easytier_proto::api::manage::VpnRuleConfig {
                    cidr: r.cidr.to_string(),
                    exit_addr: r.exit.map(|exit| exit.to_string()).unwrap_or_default(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rule(cidr: &str, exit: &str) -> VpnRuleConfig {
        VpnRuleConfig {
            cidr: cidr.parse().unwrap(),
            exit: Some(exit.parse().unwrap()),
        }
    }
    #[test]
    fn rule_exit_is_optional_in_toml() {
        let c: VpnConfig = toml::from_str("[[rules]]\ncidr = '0.0.0.0/0'\n").unwrap();
        assert_eq!(c.rules[0].exit, None);
        assert!(c.follows_exit_nodes());
        assert!(c.exit_addresses().is_empty());
        c.validate().unwrap();
        let text = toml::to_string(&c).unwrap();
        assert!(!text.contains("exit"));
        assert_eq!(toml::from_str::<VpnConfig>(&text).unwrap(), c);
    }
    #[test]
    fn defaults_and_toml_roundtrip() {
        let mut c: VpnConfig = toml::from_str("").unwrap();
        assert!(!c.client_active());
        c.rules = vec![
            rule("0.0.0.0/0", "10.0.0.2"),
            rule("2001:db8::/32", "fd00::3"),
        ];
        c.validate().unwrap();
        assert!(c.client_active());
        assert_eq!(
            toml::from_str::<VpnConfig>(&toml::to_string(&c).unwrap()).unwrap(),
            c
        );
    }
    #[test]
    fn invalid_exits_and_duplicate_subnets_are_rejected() {
        for exit in ["0.0.0.0", "::", "127.0.0.1", "::1", "224.0.0.1", "ff02::1"] {
            assert!(
                VpnConfig {
                    rules: vec![rule("::/0", exit)],
                }
                .validate()
                .is_err()
            );
        }
        let config = VpnConfig {
            rules: vec![
                rule("10.0.0.0/8", "10.1.0.2"),
                rule("10.0.0.0/8", "10.1.0.3"),
            ],
        };
        assert!(config.validate().is_err());
    }
    #[test]
    fn longest_prefix_wins_in_both_families() {
        let c = VpnConfig {
            rules: vec![
                rule("0.0.0.0/0", "10.0.0.2"),
                rule("8.0.0.0/8", "10.0.0.3"),
                rule("::/0", "fd00::2"),
                rule("2001:db8::/32", "fd00::3"),
            ],
        };
        assert_eq!(c.match_rule_index(&"8.8.8.8".parse().unwrap()), Some(1));
        assert_eq!(c.match_rule_index(&"2001:db8::1".parse().unwrap()), Some(3));
    }
    #[test]
    fn removed_fields_are_rejected() {
        for field in [
            "enabled = true",
            "fail_mode = 'direct'",
            "exits = []",
            "dns_protect = true",
            "ipv6_protect = true",
            "ignore_private = true",
        ] {
            assert!(toml::from_str::<VpnConfig>(field).is_err(), "{field}");
        }
        for field in [
            "action = 'exit'",
            "priority = 0",
            "exits = ['10.0.0.2']",
            "fallback_exits = []",
        ] {
            let text = format!("[[rules]]\ncidr = '0.0.0.0/0'\nexit = '10.0.0.2'\n{field}");
            assert!(toml::from_str::<VpnConfig>(&text).is_err(), "{field}");
        }
    }
    #[test]
    fn legacy_exit_enabled_is_read_but_enables_nothing() {
        for value in ["true", "false"] {
            let c: VpnConfig = toml::from_str(&format!("exit_enabled = {value}")).unwrap();
            assert_eq!(c, VpnConfig::default());
            assert!(!toml::to_string(&c).unwrap().contains("exit_enabled"));
        }
        assert!(toml::from_str::<VpnConfig>("exit_forwarding = true").is_err());
    }
    #[test]
    fn config_loader_rejects_duplicate_rules_before_runtime_start() {
        let text = "[[vpn.rules]]\ncidr = '203.0.113.0/24'\nexit = '10.0.0.2'\n[[vpn.rules]]\ncidr = '203.0.113.0/24'\nexit = '10.0.0.3'";
        let error = crate::config::toml::TomlConfigLoader::new_from_str(text)
            .err()
            .unwrap();
        assert!(error.to_string().contains("duplicate VPN rule CIDR"));
    }
    #[cfg(any(feature = "web-client", feature = "browser-config"))]
    #[test]
    fn protobuf_roundtrip_and_legacy_json_rejection() {
        let c = VpnConfig {
            rules: vec![rule("203.0.113.0/24", "10.0.0.2")],
        };
        assert_eq!(pb::from_pb(pb::to_pb(&c)).unwrap(), c);
        let follows = VpnConfig {
            rules: vec![VpnRuleConfig {
                cidr: "0.0.0.0/0".parse().unwrap(),
                exit: None,
            }],
        };
        assert_eq!(pb::from_pb(pb::to_pb(&follows)).unwrap(), follows);
        assert!(
            pb::from_pb(easytier_proto::api::manage::VpnConfig {
                rules: vec![easytier_proto::api::manage::VpnRuleConfig {
                    cidr: "0.0.0.0/0".into(),
                    exit_addr: "exit.example".into(),
                }],
            })
            .is_err()
        );
        assert!(
            serde_json::from_str::<easytier_proto::api::manage::VpnConfig>(
                r#"{"dns_protect":true}"#
            )
            .is_err()
        );
    }
}
