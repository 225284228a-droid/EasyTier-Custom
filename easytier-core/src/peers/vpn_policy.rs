//! Client-side multi-exit VPN policy engine.
//!
//! The engine owns the compiled rule set from [`crate::config::vpn::VpnConfig`]
//! and answers one question for every outbound destination: tunnel it through
//! which exit, keep it direct, or drop it. Rule matching is longest-prefix
//! first with priority and rule order breaking ties. Exit liveness is
//! refreshed externally (by the peer manager from route snapshots) so the
//! per-packet path stays synchronous and allocation-free.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;

use crate::config::vpn::{VpnConfig, VpnRuleAction};
use crate::config::PeerId;

/// Decision produced for one outbound destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpnDecision {
    /// No rule applies (VPN disabled or destination unmatched); keep the
    /// pre-VPN behavior for this destination.
    Passthrough,
    /// The destination is excluded from the tunnel; leave it to the
    /// underlying network.
    Direct,
    /// Drop the packet.
    Block,
    /// Send the packet through the given exit peer; the packet must carry the
    /// VPN-exit header flag so the exit side accepts and NATs it.
    Exit(PeerId),
}

/// Per-rule traffic counters, kept across status snapshots.
#[derive(Debug, Default)]
pub struct VpnRuleStats {
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl VpnRuleStats {
    pub fn record(&self, bytes: u32) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(u64::from(bytes), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.packets.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

/// Per-exit packet counters, shared with status snapshots.
#[derive(Debug, Default)]
pub struct VpnExitCounters {
    tx_packets: AtomicU64,
    tx_bytes: AtomicU64,
}

impl VpnExitCounters {
    pub fn record(&self, bytes: u32) {
        self.tx_packets.fetch_add(1, Ordering::Relaxed);
        self.tx_bytes.fetch_add(u64::from(bytes), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.tx_packets.load(Ordering::Relaxed),
            self.tx_bytes.load(Ordering::Relaxed),
        )
    }
}

/// The refresh task's liveness view of one configured exit. The map value is
/// mutated in place; the counters inside are shared so hot updates keep them.
#[derive(Debug, Default, Clone)]
pub struct VpnExitRuntimeState {
    pub peer_id: Option<PeerId>,
    /// The exit peer currently has a usable route.
    pub up: bool,
    pub latency_ms: Option<u32>,
    pub counters: Arc<VpnExitCounters>,
}

struct VpnCompiledRule {
    config: crate::config::vpn::VpnRuleConfig,
    stats: Arc<VpnRuleStats>,
}

/// Immutable compiled state; swapped atomically on config updates.
struct VpnCompiledState {
    config: VpnConfig,
    rules: Vec<VpnCompiledRule>,
}

impl VpnCompiledState {
    /// Longest-prefix match; ties are broken by lower priority, then by rule
    /// order.
    fn best_match(candidates: impl Iterator<Item = usize>, rules: &[VpnCompiledRule]) -> Option<usize> {
        candidates.max_by(|&a, &b| {
            let ra = &rules[a].config;
            let rb = &rules[b].config;
            ra.cidr
                .prefix_len()
                .cmp(&rb.cidr.prefix_len())
                .then(rb.priority.cmp(&ra.priority))
                .then(b.cmp(&a))
        })
    }
}

/// Snapshot of one rule for status reporting.
pub struct VpnRuleStatusEntry {
    pub cidr: String,
    pub action: &'static str,
    pub exit: Option<String>,
    pub fallback_exits: Vec<String>,
    pub priority: u32,
    pub effective_exit: Option<String>,
    pub packets: u64,
    pub bytes: u64,
}

/// Snapshot of one exit for status reporting.
pub struct VpnExitStatusEntry {
    pub name: String,
    pub node_addr: String,
    pub peer_id: Option<PeerId>,
    pub up: bool,
    pub latency_ms: Option<u32>,
    pub tx_packets: u64,
    pub tx_bytes: u64,
}

/// The shared policy engine. Cheap to clone through `Arc`.
pub struct VpnPolicyEngine {
    state: ArcSwap<VpnCompiledState>,
    exit_states: DashMap<String, VpnExitRuntimeState>,
}

impl VpnPolicyEngine {
    pub fn new(config: VpnConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let engine = Self {
            state: ArcSwap::from_pointee(Self::compile(config)),
            exit_states: DashMap::new(),
        };
        engine.sync_exit_state_slots();
        Ok(engine)
    }

    fn compile(config: VpnConfig) -> VpnCompiledState {
        let rules = config
            .rules
            .iter()
            .map(|rule| VpnCompiledRule {
                config: rule.clone(),
                stats: Arc::new(VpnRuleStats::default()),
            })
            .collect();
        VpnCompiledState { config, rules }
    }

    /// Hot-update the rule set. Counters restart because rules are new
    /// objects; exit liveness slots are reused by name when possible.
    pub fn install(&self, config: VpnConfig) -> anyhow::Result<()> {
        config.validate()?;
        self.state.store(Arc::new(Self::compile(config)));
        self.sync_exit_state_slots();
        Ok(())
    }

    /// Keep one liveness slot per configured exit, dropping removed ones and
    /// preserving counters of unchanged exits.
    fn sync_exit_state_slots(&self) {
        let state = self.state.load();
        let mut keep = std::collections::BTreeSet::new();
        for exit in &state.config.exits {
            keep.insert(exit.name.clone());
            self.exit_states
                .entry(exit.name.clone())
                .or_insert_with(VpnExitRuntimeState::default);
        }
        self.exit_states.retain(|name, _| keep.contains(name));
    }

    /// Replace the liveness view; called by the peer manager refresh task.
    pub fn update_exit_states<I, S>(&self, updates: I)
    where
        I: IntoIterator<Item = (S, Option<PeerId>, bool, Option<u32>)>,
        S: Into<String>,
    {
        for (name, peer_id, up, latency_ms) in updates {
            if let Some(mut slot) = self.exit_states.get_mut(&name.into()) {
                slot.peer_id = peer_id;
                slot.up = up;
                slot.latency_ms = latency_ms;
            }
        }
    }

    pub fn config(&self) -> VpnConfig {
        self.state.load().config.clone()
    }

    fn exit_state(&self, name: &str) -> Option<VpnExitRuntimeState> {
        self.exit_states.get(name).map(|s| s.clone())
    }

    /// Resolve the policy decision for one destination.
    pub fn resolve(&self, dst: &IpAddr, bytes: u32) -> VpnDecision {
        let state = self.state.load();
        if !state.config.client_active() {
            return VpnDecision::Passthrough;
        }

        let Some(rule) = state.match_rule(dst) else {
            return VpnDecision::Passthrough;
        };

        match rule.config.action {
            VpnRuleAction::Direct => {
                rule.stats.record(bytes);
                VpnDecision::Direct
            }
            VpnRuleAction::Block => {
                rule.stats.record(bytes);
                VpnDecision::Block
            }
            VpnRuleAction::Exit => {
                rule.stats.record(bytes);
                let mut candidates: Vec<&String> = Vec::new();
                if let Some(exit) = &rule.config.exit {
                    candidates.push(exit);
                }
                candidates.extend(rule.config.fallback_exits.iter());
                for name in candidates {
                    if let Some(slot) = self.exit_state(name)
                        && slot.up
                        && let Some(peer_id) = slot.peer_id
                    {
                        slot.counters.record(bytes);
                        return VpnDecision::Exit(peer_id);
                    }
                }
                match state.config.fail_mode {
                    crate::config::vpn::VpnFailMode::Direct => VpnDecision::Direct,
                    crate::config::vpn::VpnFailMode::Block => VpnDecision::Block,
                }
            }
        }
    }

    /// Rule + exit status snapshot for reporting.
    pub fn status(&self) -> (Vec<VpnRuleStatusEntry>, Vec<VpnExitStatusEntry>) {
        let state = self.state.load();
        let effective_exit = |rule: &VpnCompiledRule| -> Option<String> {
            if rule.config.action != VpnRuleAction::Exit {
                return None;
            }
            let mut candidates: Vec<&String> = Vec::new();
            if let Some(exit) = &rule.config.exit {
                candidates.push(exit);
            }
            candidates.extend(rule.config.fallback_exits.iter());
            for name in candidates {
                if let Some(slot) = self.exit_states.get(name)
                    && slot.up
                    && slot.peer_id.is_some()
                {
                    return Some(name.clone());
                }
            }
            None
        };

        let rules = state
            .rules
            .iter()
            .map(|rule| {
                let (packets, bytes) = rule.stats.snapshot();
                VpnRuleStatusEntry {
                    cidr: rule.config.cidr.to_string(),
                    action: rule.config.action.as_str(),
                    exit: rule.config.exit.clone(),
                    fallback_exits: rule.config.fallback_exits.clone(),
                    priority: rule.config.priority,
                    effective_exit: effective_exit(rule),
                    packets,
                    bytes,
                }
            })
            .collect();

        let exits = state
            .config
            .exits
            .iter()
            .map(|exit| {
                let slot = self
                    .exit_states
                    .get(&exit.name)
                    .map(|s| s.clone())
                    .unwrap_or_default();
                let (tx_packets, tx_bytes) = slot.counters.snapshot();
                VpnExitStatusEntry {
                    name: exit.name.clone(),
                    node_addr: exit.node.to_string(),
                    peer_id: slot.peer_id,
                    up: slot.up,
                    latency_ms: slot.latency_ms,
                    tx_packets,
                    tx_bytes,
                }
            })
            .collect();

        (rules, exits)
    }
}

impl VpnCompiledState {
    fn match_rule(&self, dst: &IpAddr) -> Option<&VpnCompiledRule> {
        let idx = match dst {
            IpAddr::V4(v4) => self.match_v4(v4)?,
            IpAddr::V6(v6) => self.match_v6(v6)?,
        };
        self.rules.get(idx)
    }

    fn match_v4(&self, dst: &Ipv4Addr) -> Option<usize> {
        if !self.config.client_active() {
            return None;
        }
        let candidates = self.rules.iter().enumerate().filter_map(|(idx, rule)| {
            let crate::config::vpn::VpnCidr::V4(cidr) = rule.config.cidr else {
                return None;
            };
            cidr.contains(dst).then_some(idx)
        });
        VpnCompiledState::best_match(candidates, &self.rules)
    }

    fn match_v6(&self, dst: &Ipv6Addr) -> Option<usize> {
        if !self.config.client_active() {
            return None;
        }
        let candidates = self.rules.iter().enumerate().filter_map(|(idx, rule)| {
            let crate::config::vpn::VpnCidr::V6(cidr) = rule.config.cidr else {
                return None;
            };
            cidr.contains(dst).then_some(idx)
        });
        VpnCompiledState::best_match(candidates, &self.rules)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::vpn::{VpnExitConfig, VpnRuleConfig};

    fn engine_with(rules: Vec<VpnRuleConfig>, exits: Vec<VpnExitConfig>) -> VpnPolicyEngine {
        VpnPolicyEngine::new(VpnConfig {
            enabled: true,
            exits,
            rules,
            ..Default::default()
        })
        .unwrap()
    }

    fn rule(cidr: &str, action: VpnRuleAction, exit: Option<&str>) -> VpnRuleConfig {
        VpnRuleConfig {
            cidr: cidr.parse().unwrap(),
            action,
            exit: exit.map(ToString::to_string),
            fallback_exits: vec![],
            priority: 0,
        }
    }

    #[test]
    fn longest_prefix_wins() {
        let engine = engine_with(
            vec![
                rule("0.0.0.0/0", VpnRuleAction::Exit, Some("a")),
                rule("10.0.0.0/8", VpnRuleAction::Block, None),
                rule("10.1.0.0/16", VpnRuleAction::Direct, None),
            ],
            vec![VpnExitConfig {
                name: "a".into(),
                node: "10.144.144.2".parse().unwrap(),
                note: None,
            }],
        );
        engine.update_exit_states([("a", Some(7u32), true, Some(12))]);

        assert_eq!(
            engine.resolve(&"8.8.8.8".parse().unwrap(), 100),
            VpnDecision::Exit(7)
        );
        assert_eq!(
            engine.resolve(&"10.2.0.1".parse().unwrap(), 100),
            VpnDecision::Block
        );
        assert_eq!(
            engine.resolve(&"10.1.0.1".parse().unwrap(), 100),
            VpnDecision::Direct
        );
    }

    #[test]
    fn priority_breaks_equal_prefix_ties() {
        let mut lo = rule("192.168.0.0/16", VpnRuleAction::Direct, None);
        lo.priority = 10;
        let hi = rule("192.168.0.0/16", VpnRuleAction::Block, None);
        let engine = engine_with(vec![lo, hi], vec![]);
        assert_eq!(
            engine.resolve(&"192.168.1.1".parse().unwrap(), 64),
            VpnDecision::Block
        );
    }

    #[test]
    fn ipv6_rules_match() {
        let mut rules = vec![rule("::/0", VpnRuleAction::Exit, Some("a"))];
        rules.push(rule("fd00:abcd::/32", VpnRuleAction::Block, None));
        let engine = engine_with(
            rules,
            vec![VpnExitConfig {
                name: "a".into(),
                node: "10.144.144.2".parse().unwrap(),
                note: None,
            }],
        );
        engine.update_exit_states([("a", Some(9u32), true, None)]);
        assert_eq!(
            engine.resolve(&"2001:db8::1".parse().unwrap(), 64),
            VpnDecision::Exit(9)
        );
        assert_eq!(
            engine.resolve(&"fd00:abcd::1".parse().unwrap(), 64),
            VpnDecision::Block
        );
    }

    #[test]
    fn fallback_used_when_primary_down() {
        let mut r = rule("0.0.0.0/0", VpnRuleAction::Exit, Some("a"));
        r.fallback_exits = vec!["b".into()];
        let engine = engine_with(
            vec![r],
            vec![
                VpnExitConfig {
                    name: "a".into(),
                    node: "10.144.144.2".parse().unwrap(),
                    note: None,
                },
                VpnExitConfig {
                    name: "b".into(),
                    node: "10.144.144.3".parse().unwrap(),
                    note: None,
                },
            ],
        );
        engine.update_exit_states([("a", Some(1u32), false, None), ("b", Some(2u32), true, None)]);
        assert_eq!(
            engine.resolve(&"1.1.1.1".parse().unwrap(), 10),
            VpnDecision::Exit(2)
        );
    }

    #[test]
    fn all_exits_down_honors_fail_mode() {
        let r = rule("0.0.0.0/0", VpnRuleAction::Exit, Some("a"));
        let mut engine = engine_with(
            vec![r.clone()],
            vec![VpnExitConfig {
                name: "a".into(),
                node: "10.144.144.2".parse().unwrap(),
                note: None,
            }],
        );
        engine.update_exit_states([("a", Some(1u32), false, None)]);
        assert_eq!(
            engine.resolve(&"1.1.1.1".parse().unwrap(), 10),
            VpnDecision::Direct
        );

        let mut config = engine.config();
        config.fail_mode = crate::config::vpn::VpnFailMode::Block;
        engine.install(config).unwrap();
        engine.update_exit_states([("a", Some(1u32), false, None)]);
        assert_eq!(
            engine.resolve(&"1.1.1.1".parse().unwrap(), 10),
            VpnDecision::Block
        );
    }

    #[test]
    fn disabled_engine_passes_through() {
        let engine = VpnPolicyEngine::new(VpnConfig::default()).unwrap();
        assert_eq!(
            engine.resolve(&"1.2.3.4".parse().unwrap(), 10),
            VpnDecision::Passthrough
        );
        let (rules, exits) = engine.status();
        assert!(rules.is_empty());
        assert!(exits.is_empty());
    }

    #[test]
    fn hot_update_swaps_rules_and_keeps_exit_counters() {
        let engine = engine_with(
            vec![rule("0.0.0.0/0", VpnRuleAction::Exit, Some("a"))],
            vec![VpnExitConfig {
                name: "a".into(),
                node: "10.144.144.2".parse().unwrap(),
                note: None,
            }],
        );
        engine.update_exit_states([("a", Some(4u32), true, None)]);
        engine.resolve(&"1.1.1.1".parse().unwrap(), 500);

        let mut config = engine.config();
        config.rules.clear();
        config.rules.push(rule("8.8.8.0/24", VpnRuleAction::Exit, Some("a")));
        engine.install(config).unwrap();

        // counters on the exit slot survive the swap
        let (_, exits) = engine.status();
        assert_eq!(exits[0].tx_packets, 1);
        assert_eq!(exits[0].tx_bytes, 500);
        // new rule set: 1.1.1.1 no longer matches
        assert_eq!(
            engine.resolve(&"1.1.1.1".parse().unwrap(), 10),
            VpnDecision::Passthrough
        );
        assert_eq!(
            engine.resolve(&"8.8.8.8".parse().unwrap(), 10),
            VpnDecision::Exit(4)
        );
    }
}

/// Host-side route-manager state, reported for status RPCs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VpnHostRouteState {
    /// The route manager is running with an active client policy.
    pub routes_active: bool,
    pub routes_installed: usize,
    pub protected_endpoints: Vec<String>,
    pub system_dns_servers: Vec<std::net::IpAddr>,
}

/// Host adapter exposing route-manager state to the status RPC.
pub trait VpnRouteStateHost: Send + Sync {
    fn vpn_route_state(&self) -> Option<VpnHostRouteState> {
        None
    }
}

impl VpnRouteStateHost for () {}
