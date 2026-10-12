//! Configuration model for the multi-exit policy VPN.
//!
//! One `[vpn]` TOML section replaces both the legacy exit-node list and the
//! legacy public-IPv6 provider. A node may reference several exit nodes and
//! split traffic per destination CIDR; the client-side policy engine and the
//! host-side route manager both derive their behavior from this model.

use std::net::IpAddr;
use std::str::FromStr;

use cidr::{Ipv4Cidr, Ipv6Cidr};
use serde::{Deserialize, Serialize};

/// What a client does when every exit of a rule is unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VpnFailMode {
    /// Fail open: while every exit of a rule is unreachable, the rule's
    /// takeover routes are withdrawn from the system routing table so
    /// matching traffic uses the physical network. Packets that still enter
    /// the tunnel in that state are dropped.
    #[default]
    Direct,
    /// Fail closed: traffic is dropped while no exit is reachable.
    Block,
}

impl VpnFailMode {
    pub fn as_str(self) -> &'static str {
        match self {
            VpnFailMode::Direct => "direct",
            VpnFailMode::Block => "block",
        }
    }
}

/// Action attached to one VPN rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VpnRuleAction {
    /// Route matching destinations through the selected exit node.
    #[default]
    Exit,
    /// Keep matching destinations on the underlying network (exclusion
    /// routes; more specific than the tunneled default route).
    Direct,
    /// Drop matching destinations (blackhole routes plus packet drop).
    Block,
}

impl VpnRuleAction {
    pub fn as_str(self) -> &'static str {
        match self {
            VpnRuleAction::Exit => "exit",
            VpnRuleAction::Direct => "direct",
            VpnRuleAction::Block => "block",
        }
    }
}

/// A destination prefix, IPv4 or IPv6. Serialized as its `a.b.c.d/len`
/// string form in TOML/JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpnCidr {
    V4(Ipv4Cidr),
    V6(Ipv6Cidr),
}

impl<'de> Deserialize<'de> for VpnCidr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for VpnCidr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl VpnCidr {
    pub fn is_ipv4(&self) -> bool {
        matches!(self, VpnCidr::V4(_))
    }

    /// The wildcard default-route prefixes `0.0.0.0/0` and `::/0`.
    pub fn is_default_route(&self) -> bool {
        match self {
            VpnCidr::V4(cidr) => cidr.network_length() == 0,
            VpnCidr::V6(cidr) => cidr.network_length() == 0,
        }
    }

    pub fn prefix_len(&self) -> u8 {
        match self {
            VpnCidr::V4(cidr) => cidr.network_length(),
            VpnCidr::V6(cidr) => cidr.network_length(),
        }
    }
}

impl std::fmt::Display for VpnCidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VpnCidr::V4(cidr) => write!(f, "{cidr}"),
            VpnCidr::V6(cidr) => write!(f, "{cidr}"),
        }
    }
}

impl FromStr for VpnCidr {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(cidr) = Ipv4Cidr::from_str(s) {
            return Ok(VpnCidr::V4(cidr));
        }
        if let Ok(cidr) = Ipv6Cidr::from_str(s) {
            return Ok(VpnCidr::V6(cidr));
        }
        anyhow::bail!("invalid VPN rule CIDR: {s:?}")
    }
}

/// One exit node reference. `node` is the exit node's virtual address inside
/// the EasyTier network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VpnExitConfig {
    pub name: String,
    pub node: IpAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One traffic-splitting rule. Matching uses the longest prefix first; equal
/// prefix lengths are disambiguated by lower `priority`, then by list order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VpnRuleConfig {
    pub cidr: VpnCidr,
    #[serde(default)]
    pub action: VpnRuleAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_exits: Vec<String>,
    #[serde(default)]
    pub priority: u32,
}

/// The complete `[vpn]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VpnConfig {
    /// Enable this node as a VPN client: apply rules, routes and leak
    /// protection.
    #[serde(default)]
    pub enabled: bool,
    /// Offer exit forwarding (NAT) for other nodes' tunneled traffic.
    #[serde(default)]
    pub exit_enabled: bool,
    /// Pin system DNS servers into the tunnel while a global IPv4 exit is
    /// active, so queries cannot bypass the exit.
    #[serde(default = "default_true")]
    pub dns_protect: bool,
    /// Blackhole non-mesh IPv6 while no global IPv6 exit is active, so v6
    /// traffic cannot leak around an IPv4-only exit.
    #[serde(default = "default_true")]
    pub ipv6_protect: bool,
    #[serde(default)]
    pub fail_mode: VpnFailMode,
    #[serde(default)]
    pub exits: Vec<VpnExitConfig>,
    #[serde(default)]
    pub rules: Vec<VpnRuleConfig>,
}

fn default_true() -> bool {
    true
}

impl Default for VpnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            exit_enabled: false,
            dns_protect: true,
            ipv6_protect: true,
            fail_mode: VpnFailMode::Direct,
            exits: Vec::new(),
            rules: Vec::new(),
        }
    }
}

impl VpnConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut names = std::collections::BTreeSet::new();
        for exit in &self.exits {
            if exit.name.trim().is_empty() {
                anyhow::bail!("vpn exit with empty name");
            }
            if !names.insert(exit.name.clone()) {
                anyhow::bail!("duplicate vpn exit name: {}", exit.name);
            }
            if exit.node.is_unspecified() || exit.node.is_multicast() {
                anyhow::bail!("vpn exit {} has invalid node address", exit.name);
            }
        }

        for rule in &self.rules {
            if let VpnRuleAction::Exit = rule.action {
                let Some(exit) = &rule.exit else {
                    anyhow::bail!("vpn rule {} needs an exit name", rule.cidr);
                };
                if !names.contains(exit) {
                    anyhow::bail!("vpn rule {} references unknown exit {exit}", rule.cidr);
                }
                for fallback in &rule.fallback_exits {
                    if !names.contains(fallback) {
                        anyhow::bail!(
                            "vpn rule {} references unknown fallback exit {fallback}",
                            rule.cidr
                        );
                    }
                    if fallback == exit {
                        anyhow::bail!(
                            "vpn rule {} lists exit {exit} as its own fallback",
                            rule.cidr
                        );
                    }
                }
            }
        }

        if self.enabled && self.rules.iter().any(|r| r.action == VpnRuleAction::Exit) {
            for exit in &self.exits {
                let referenced = self.rules.iter().any(|r| {
                    r.action == VpnRuleAction::Exit
                        && (r.exit.as_deref() == Some(exit.name.as_str())
                            || r.fallback_exits.iter().any(|f| f == &exit.name))
                });
                if !referenced {
                    tracing::warn!(exit = %exit.name, "vpn exit is not referenced by any rule");
                }
            }
        }

        Ok(())
    }

    pub fn find_exit(&self, name: &str) -> Option<&VpnExitConfig> {
        self.exits.iter().find(|exit| exit.name == name)
    }

    /// Whether an enabled exit rule covers the whole IPv4 internet.
    pub fn has_global_v4_exit(&self) -> bool {
        self.enabled
            && self.rules.iter().any(|r| {
                r.action == VpnRuleAction::Exit
                    && matches!(r.cidr, VpnCidr::V4(c) if c.network_length() == 0)
            })
    }

    /// Whether an enabled exit rule covers the whole IPv6 internet.
    pub fn has_global_v6_exit(&self) -> bool {
        self.enabled
            && self.rules.iter().any(|r| {
                r.action == VpnRuleAction::Exit
                    && matches!(r.cidr, VpnCidr::V6(c) if c.network_length() == 0)
            })
    }

    /// Whether any enabled rule routes IPv4 destinations through an exit.
    pub fn has_v4_exit_rule(&self) -> bool {
        self.enabled
            && self
                .rules
                .iter()
                .any(|r| r.action == VpnRuleAction::Exit && r.cidr.is_ipv4())
    }

    /// Whether any enabled rule routes IPv6 destinations through an exit.
    pub fn has_v6_exit_rule(&self) -> bool {
        self.enabled
            && self
                .rules
                .iter()
                .any(|r| r.action == VpnRuleAction::Exit && !r.cidr.is_ipv4())
    }

    /// Client behavior is active when enabled and at least one rule exists.
    pub fn client_active(&self) -> bool {
        self.enabled && !self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_vpn(toml_src: &str) -> VpnConfig {
        toml::from_str(toml_src).unwrap()
    }

    #[test]
    fn parses_full_section() {
        let cfg = parse_vpn(
            r#"
enabled = true
exit_enabled = false
dns_protect = false
ipv6_protect = false
fail_mode = "block"

[[exits]]
name = "hk"
node = "10.144.144.2"

[[exits]]
name = "us"
node = "10.144.144.3"
note = "fallback"

[[rules]]
cidr = "0.0.0.0/0"
action = "exit"
exit = "hk"
fallback_exits = ["us"]

[[rules]]
cidr = "192.168.0.0/16"
action = "direct"
priority = 10

[[rules]]
cidr = "10.0.0.0/8"
action = "block"

[[rules]]
cidr = "::/0"
action = "exit"
exit = "hk"
"#,
        );
        assert!(cfg.enabled);
        assert!(!cfg.dns_protect);
        assert_eq!(cfg.fail_mode, VpnFailMode::Block);
        assert_eq!(cfg.exits.len(), 2);
        assert_eq!(cfg.rules.len(), 4);
        assert!(cfg.has_global_v4_exit());
        assert!(cfg.has_global_v6_exit());
        cfg.validate().unwrap();
    }

    #[test]
    fn defaults_are_safe() {
        let cfg: VpnConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.dns_protect);
        assert!(cfg.ipv6_protect);
        assert_eq!(cfg.fail_mode, VpnFailMode::Direct);
        assert!(!cfg.has_global_v4_exit());
    }

    #[test]
    fn rejects_unknown_exit_reference() {
        let cfg = parse_vpn(
            r#"
enabled = true
[[rules]]
cidr = "0.0.0.0/0"
action = "exit"
exit = "missing"
"#,
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_exit_names() {
        let cfg = parse_vpn(
            r#"
[[exits]]
name = "a"
node = "10.0.0.2"

[[exits]]
name = "a"
node = "10.0.0.3"
"#,
        );
        assert!(cfg.validate().is_err());
    }
}

/// Conversions between the TOML model and the management protobuf model.
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
        let err = |msg: String| ParseVpnError(msg);
        let fail_mode = match vpn.fail_mode.as_str() {
            "" | "direct" => VpnFailMode::Direct,
            "block" => VpnFailMode::Block,
            other => return Err(err(format!("invalid fail_mode {other:?}"))),
        };
        let mut exits = Vec::with_capacity(vpn.exits.len());
        for exit in vpn.exits {
            let node: IpAddr = exit
                .node
                .parse()
                .map_err(|e| err(format!("exit {}: invalid node address {exit:?}: {e}", exit.name)))?;
            exits.push(VpnExitConfig {
                name: exit.name,
                node,
                note: exit.note,
            });
        }
        let mut rules = Vec::with_capacity(vpn.rules.len());
        for rule in vpn.rules {
            let cidr: VpnCidr = rule
                .cidr
                .parse()
                .map_err(|e| err(format!("rule {:?}: invalid CIDR: {e}", rule.cidr)))?;
            let action = match rule.action.as_str() {
                "" | "exit" => VpnRuleAction::Exit,
                "direct" => VpnRuleAction::Direct,
                "block" => VpnRuleAction::Block,
                other => return Err(err(format!("rule {cidr}: invalid action {other:?}"))),
            };
            rules.push(VpnRuleConfig {
                cidr,
                action,
                exit: rule.exit,
                fallback_exits: rule.fallback_exits,
                priority: rule.priority.unwrap_or_default(),
            });
        }
        Ok(VpnConfig {
            enabled: vpn.enabled,
            exit_enabled: vpn.exit_enabled,
            dns_protect: vpn.dns_protect,
            ipv6_protect: vpn.ipv6_protect,
            fail_mode,
            exits,
            rules,
        })
    }

    pub fn to_pb(vpn: &VpnConfig) -> easytier_proto::api::manage::VpnConfig {
        easytier_proto::api::manage::VpnConfig {
            enabled: vpn.enabled,
            exit_enabled: vpn.exit_enabled,
            dns_protect: vpn.dns_protect,
            ipv6_protect: vpn.ipv6_protect,
            fail_mode: vpn.fail_mode.as_str().to_owned(),
            exits: vpn
                .exits
                .iter()
                .map(|exit| easytier_proto::api::manage::VpnExitConfig {
                    name: exit.name.clone(),
                    node: exit.node.to_string(),
                    note: exit.note.clone(),
                })
                .collect(),
            rules: vpn
                .rules
                .iter()
                .map(|rule| easytier_proto::api::manage::VpnRuleConfig {
                    cidr: rule.cidr.to_string(),
                    action: rule.action.as_str().to_owned(),
                    exit: rule.exit.clone(),
                    fallback_exits: rule.fallback_exits.clone(),
                    priority: Some(rule.priority),
                })
                .collect(),
        }
    }
}
