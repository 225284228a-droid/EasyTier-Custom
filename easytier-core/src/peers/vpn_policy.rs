//! Shared packet decisions, address exclusions, and ordered exit liveness.
use crate::config::{
    PeerId,
    vpn::{
        VpnConfig, VpnRuleAction,
        routes::{VpnRouteExclusions, VpnRoutePlan, compile_vpn_routes, is_private_address},
    },
};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::{
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpnDecision {
    Passthrough,
    Direct,
    Block,
    Exit(PeerId),
}

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
#[derive(Debug, Default, Clone)]
pub struct VpnExitRuntimeState {
    pub peer_id: Option<PeerId>,
    pub up: bool,
    pub latency_ms: Option<u32>,
    pub counters: Arc<VpnExitCounters>,
}
struct VpnCompiledState {
    config: VpnConfig,
    stats: Vec<Arc<VpnRuleStats>>,
    exclusions: VpnRouteExclusions,
    dns_servers: Vec<IpAddr>,
    system_dns_servers: Vec<IpAddr>,
    forward_dns_servers: Vec<IpAddr>,
    plan: VpnRoutePlan,
    generation: u64,
}
pub struct VpnRuleStatusEntry {
    pub cidr: String,
    pub action: &'static str,
    pub exits: Vec<String>,
    pub priority: u32,
    pub effective_exit: Option<String>,
    pub packets: u64,
    pub bytes: u64,
}
pub struct VpnExitStatusEntry {
    pub node_addr: String,
    pub peer_id: Option<PeerId>,
    pub up: bool,
    pub latency_ms: Option<u32>,
    pub tx_packets: u64,
    pub tx_bytes: u64,
}
pub struct VpnPolicyEngine {
    state: ArcSwap<VpnCompiledState>,
    exit_states: DashMap<IpAddr, VpnExitRuntimeState>,
    update_lock: Mutex<()>,
}
impl VpnPolicyEngine {
    pub fn new(config: VpnConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let stats = config
            .rules
            .iter()
            .map(|_| Arc::new(VpnRuleStats::default()))
            .collect();
        let exclusions = VpnRouteExclusions::default();
        let plan = compile_vpn_routes(&config, &exclusions, &[]);
        let engine = Self {
            state: ArcSwap::from_pointee(VpnCompiledState {
                config,
                stats,
                exclusions,
                dns_servers: vec![],
                system_dns_servers: vec![],
                forward_dns_servers: vec![],
                plan,
                generation: 1,
            }),
            exit_states: DashMap::new(),
            update_lock: Mutex::new(()),
        };
        engine.sync_exit_state_slots();
        Ok(engine)
    }
    pub fn install(&self, config: VpnConfig) -> anyhow::Result<()> {
        config.validate()?;
        let _guard = self.update_lock.lock().unwrap();
        let old = self.state.load_full();
        if old.config == config {
            return Ok(());
        }
        let mut matched_old_rules = vec![false; old.config.rules.len()];
        let stats = config
            .rules
            .iter()
            .map(|r| {
                old.config
                    .rules
                    .iter()
                    .enumerate()
                    .find(|(i, o)| !matched_old_rules[*i] && *o == r)
                    .map(|(i, _)| {
                        matched_old_rules[i] = true;
                        old.stats[i].clone()
                    })
                    .unwrap_or_default()
            })
            .collect();
        let plan = compile_vpn_routes(&config, &old.exclusions, &old.dns_servers);
        self.state.store(Arc::new(VpnCompiledState {
            config,
            stats,
            exclusions: old.exclusions.clone(),
            dns_servers: old.dns_servers.clone(),
            system_dns_servers: old.system_dns_servers.clone(),
            forward_dns_servers: old.forward_dns_servers.clone(),
            plan,
            generation: old.generation.wrapping_add(1),
        }));
        self.sync_exit_state_slots();
        Ok(())
    }
    fn sync_exit_state_slots(&self) {
        let addresses = self.state.load().config.exit_addresses();
        for a in &addresses {
            self.exit_states.entry(*a).or_default();
        }
        self.exit_states.retain(|a, _| addresses.contains(a));
    }
    pub fn update_exit_states<I>(&self, updates: I)
    where
        I: IntoIterator<Item = (IpAddr, Option<PeerId>, bool, Option<u32>)>,
    {
        for (a, peer_id, up, latency_ms) in updates {
            if let Some(mut slot) = self.exit_states.get_mut(&a) {
                slot.peer_id = peer_id;
                slot.up = up;
                slot.latency_ms = latency_ms;
            }
        }
    }
    pub fn config(&self) -> VpnConfig {
        self.state.load().config.clone()
    }
    pub fn is_overlay_destination(&self, destination: &IpAddr) -> bool {
        let state = self.state.load();
        !state
            .exclusions
            .physical
            .iter()
            .any(|c| c.contains(destination))
            && state
                .exclusions
                .overlay
                .iter()
                .any(|c| c.contains(destination))
    }
    pub fn route_plan(&self) -> (VpnRoutePlan, u64) {
        let s = self.state.load();
        (s.plan.clone(), s.generation)
    }
    pub fn update_route_context(&self, exclusions: VpnRouteExclusions, dns_servers: Vec<IpAddr>) {
        self.replace_context(Some(exclusions), Some(dns_servers), None);
    }
    pub fn update_exclusions(&self, exclusions: VpnRouteExclusions) {
        self.replace_context(Some(exclusions), None, None);
    }
    /// VPN DNS forwarders and host DNS have separate owners. A host route
    /// refresh must not withdraw a VPN DNS server's route guard.
    pub fn update_dns_servers(&self, servers: Vec<IpAddr>) {
        self.replace_context(None, None, Some(servers));
    }
    pub fn update_system_dns_servers(&self, servers: Vec<IpAddr>) {
        self.replace_context(None, Some(servers), None);
    }
    fn replace_context(
        &self,
        exclusions: Option<VpnRouteExclusions>,
        system_dns_servers: Option<Vec<IpAddr>>,
        forward_dns_servers: Option<Vec<IpAddr>>,
    ) {
        let _guard = self.update_lock.lock().unwrap();
        let old = self.state.load_full();
        let exclusions = exclusions.unwrap_or_else(|| old.exclusions.clone());
        let mut system_dns_servers =
            system_dns_servers.unwrap_or_else(|| old.system_dns_servers.clone());
        let mut forward_dns_servers =
            forward_dns_servers.unwrap_or_else(|| old.forward_dns_servers.clone());
        system_dns_servers.sort();
        system_dns_servers.dedup();
        forward_dns_servers.sort();
        forward_dns_servers.dedup();
        let mut dns_servers: Vec<_> = system_dns_servers
            .iter()
            .chain(&forward_dns_servers)
            .copied()
            .collect();
        dns_servers.sort();
        dns_servers.dedup();
        if old.exclusions == exclusions
            && old.system_dns_servers == system_dns_servers
            && old.forward_dns_servers == forward_dns_servers
        {
            return;
        }
        let generation = if old.exclusions != exclusions || old.dns_servers != dns_servers {
            old.generation.wrapping_add(1)
        } else {
            old.generation
        };
        let plan = compile_vpn_routes(&old.config, &exclusions, &dns_servers);
        self.state.store(Arc::new(VpnCompiledState {
            config: old.config.clone(),
            stats: old.stats.clone(),
            exclusions,
            dns_servers,
            system_dns_servers,
            forward_dns_servers,
            plan,
            generation,
        }));
    }
    fn selected_exit(
        &self,
        rule: &crate::config::vpn::VpnRuleConfig,
    ) -> Option<(IpAddr, VpnExitRuntimeState)> {
        rule.exits.iter().find_map(|a| {
            self.exit_states
                .get(a)
                .filter(|s| s.up && s.peer_id.is_some())
                .map(|s| (*a, s.clone()))
        })
    }
    pub fn resolve(&self, dst: &IpAddr, bytes: u32) -> VpnDecision {
        let state = self.state.load();
        // These paths retain their normal ownership even without client rules.
        if state.exclusions.physical.iter().any(|c| c.contains(dst)) {
            return VpnDecision::Direct;
        }
        if state.exclusions.overlay.iter().any(|c| c.contains(dst)) {
            return VpnDecision::Passthrough;
        }
        if !state.config.client_active() {
            return VpnDecision::Passthrough;
        }
        if state.config.ignore_private && is_private_address(dst) {
            return VpnDecision::Direct;
        }
        let Some(region) = state.plan.regions.iter().find(|r| r.cidr.contains(dst)) else {
            return VpnDecision::Passthrough;
        };
        if bytes != 0
            && let Some(i) = region.rule_index
        {
            state.stats[i].record(bytes);
        }
        match region.action {
            VpnRuleAction::Direct => VpnDecision::Direct,
            VpnRuleAction::Block => VpnDecision::Block,
            VpnRuleAction::Exit => {
                let Some(i) = region.rule_index else {
                    return VpnDecision::Block;
                };
                if let Some((_, slot)) = self.selected_exit(&state.config.rules[i]) {
                    if bytes != 0 {
                        slot.counters.record(bytes);
                    }
                    VpnDecision::Exit(slot.peer_id.unwrap())
                } else {
                    VpnDecision::Block
                }
            }
        }
    }
    pub fn status(&self) -> (Vec<VpnRuleStatusEntry>, Vec<VpnExitStatusEntry>) {
        let state = self.state.load();
        let rules = state
            .config
            .rules
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (packets, bytes) = state.stats[i].snapshot();
                VpnRuleStatusEntry {
                    cidr: r.cidr.to_string(),
                    action: r.action.as_str(),
                    exits: r.exits.iter().map(ToString::to_string).collect(),
                    priority: r.priority,
                    effective_exit: state
                        .plan
                        .regions
                        .iter()
                        .any(|region| {
                            region.rule_index == Some(i) && region.action == VpnRuleAction::Exit
                        })
                        .then(|| self.selected_exit(r))
                        .flatten()
                        .map(|(a, _)| a.to_string()),
                    packets,
                    bytes,
                }
            })
            .collect();
        let exits = state
            .config
            .exit_addresses()
            .iter()
            .map(|a| {
                let slot = self
                    .exit_states
                    .get(a)
                    .map(|s| s.clone())
                    .unwrap_or_default();
                let (tx_packets, tx_bytes) = slot.counters.snapshot();
                VpnExitStatusEntry {
                    node_addr: a.to_string(),
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VpnHostRouteState {
    pub routes_active: bool,
    pub routes_installed: usize,
    pub protected_endpoints: Vec<String>,
    pub system_dns_servers: Vec<IpAddr>,
    pub activation_error: Option<String>,
}
pub trait VpnRouteStateHost: Send + Sync {
    fn vpn_route_state(&self) -> Option<VpnHostRouteState> {
        None
    }
}
impl VpnRouteStateHost for () {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::vpn::VpnRuleConfig;
    fn rule(c: &str, action: VpnRuleAction, exits: &[&str]) -> VpnRuleConfig {
        VpnRuleConfig {
            cidr: c.parse().unwrap(),
            action,
            exits: exits.iter().map(|a| a.parse().unwrap()).collect(),
            priority: 0,
        }
    }
    fn engine(rules: Vec<VpnRuleConfig>) -> VpnPolicyEngine {
        VpnPolicyEngine::new(VpnConfig {
            ignore_private: false,
            rules,
            ..Default::default()
        })
        .unwrap()
    }
    fn state(e: &VpnPolicyEngine, a: &str, peer: u32, up: bool) {
        e.update_exit_states([(a.parse().unwrap(), Some(peer), up, None)]);
    }
    #[test]
    fn unavailable_specific_exit_never_falls_to_covering_exit() {
        let e = engine(vec![
            rule("0.0.0.0/0", VpnRuleAction::Exit, &["10.0.0.2"]),
            rule("10.0.0.0/8", VpnRuleAction::Exit, &["10.0.0.3"]),
        ]);
        state(&e, "10.0.0.2", 2, true);
        assert_eq!(
            e.resolve(&"10.1.0.1".parse().unwrap(), 1),
            VpnDecision::Block
        );
        assert_eq!(
            e.resolve(&"8.8.8.8".parse().unwrap(), 1),
            VpnDecision::Exit(2)
        );
        state(&e, "10.0.0.3", 3, true);
        assert_eq!(
            e.resolve(&"10.1.0.1".parse().unwrap(), 1),
            VpnDecision::Exit(3)
        );
    }
    #[test]
    fn ordered_exits_recover_to_first_and_keep_shared_counters() {
        let e = engine(vec![rule(
            "0.0.0.0/0",
            VpnRuleAction::Exit,
            &["10.0.0.2", "10.0.0.3"],
        )]);
        assert_eq!(
            e.resolve(&"1.1.1.1".parse().unwrap(), 1),
            VpnDecision::Block
        );
        let (before, generation) = e.route_plan();
        state(&e, "10.0.0.3", 3, true);
        assert_eq!(
            e.resolve(&"1.1.1.1".parse().unwrap(), 50),
            VpnDecision::Exit(3)
        );
        state(&e, "10.0.0.2", 2, true);
        assert_eq!(
            e.resolve(&"1.1.1.1".parse().unwrap(), 10),
            VpnDecision::Exit(2)
        );
        state(&e, "10.0.0.3", 3, false);
        state(&e, "10.0.0.2", 2, false);
        assert_eq!(
            e.resolve(&"1.1.1.1".parse().unwrap(), 1),
            VpnDecision::Block
        );
        assert_eq!(e.route_plan(), (before, generation));
        let mut c = e.config();
        c.rules[0].cidr = "1.1.1.0/24".parse().unwrap();
        e.install(c).unwrap();
        assert_eq!(
            e.status()
                .1
                .iter()
                .find(|s| s.node_addr == "10.0.0.3")
                .unwrap()
                .tx_bytes,
            50
        );
    }
    #[test]
    fn private_bypass_and_overlay_exclusions_override_all_actions() {
        let e = VpnPolicyEngine::new(VpnConfig {
            rules: vec![
                rule("::/0", VpnRuleAction::Block, &[]),
                rule("0.0.0.0/0", VpnRuleAction::Block, &[]),
            ],
            ..Default::default()
        })
        .unwrap();
        for a in [
            "10.1.0.1",
            "172.20.0.1",
            "100.64.1.1",
            "127.0.0.1",
            "169.254.1.1",
            "fd01::1",
            "fe80::1",
            "::1",
        ] {
            assert_eq!(e.resolve(&a.parse().unwrap(), 0), VpnDecision::Direct);
        }
        e.update_exclusions(VpnRouteExclusions {
            overlay: vec!["203.0.113.0/24".parse().unwrap()],
            physical: vec![],
        });
        assert_eq!(
            e.resolve(&"203.0.113.1".parse().unwrap(), 0),
            VpnDecision::Passthrough
        );
        assert_eq!(
            e.resolve(&"8.8.8.8".parse().unwrap(), 0),
            VpnDecision::Block
        );
    }
    #[test]
    fn equal_prefix_priority_then_order_and_clear_rules() {
        let mut a = rule("8.8.0.0/16", VpnRuleAction::Direct, &[]);
        a.priority = 10;
        let e = engine(vec![
            a,
            rule("8.8.0.0/16", VpnRuleAction::Block, &[]),
            rule("8.8.0.0/16", VpnRuleAction::Direct, &[]),
        ]);
        assert_eq!(
            e.resolve(&"8.8.8.8".parse().unwrap(), 0),
            VpnDecision::Block
        );
        let mut c = e.config();
        c.rules.clear();
        e.install(c).unwrap();
        assert_eq!(
            e.resolve(&"8.8.8.8".parse().unwrap(), 0),
            VpnDecision::Passthrough
        );
    }
    #[test]
    fn shadowed_exit_rules_have_no_current_exit() {
        let e = engine(vec![
            rule("8.8.8.0/24", VpnRuleAction::Block, &[]),
            rule("8.8.8.0/24", VpnRuleAction::Exit, &["10.0.0.2"]),
        ]);
        state(&e, "10.0.0.2", 2, true);
        assert!(e.status().0[1].effective_exit.is_none());
        assert_eq!(
            e.resolve(&"8.8.8.8".parse().unwrap(), 0),
            VpnDecision::Block
        );
        assert_eq!(e.status().0[0].packets, 0);
    }
    #[test]
    fn identical_rules_keep_separate_counters_across_install() {
        let repeated = rule("8.8.8.0/24", VpnRuleAction::Block, &[]);
        let e = engine(vec![repeated.clone(), repeated]);
        assert_eq!(
            e.resolve(&"8.8.8.8".parse().unwrap(), 64),
            VpnDecision::Block
        );
        let mut config = e.config();
        config.dns_protect = !config.dns_protect;
        e.install(config).unwrap();
        let (rules, _) = e.status();
        assert_eq!(rules[0].packets, 1);
        assert_eq!(rules[1].packets, 0);
    }
    #[test]
    fn guard_remains_at_start_and_after_last_exit_down() {
        let e = engine(vec![rule("0.0.0.0/0", VpnRuleAction::Exit, &["10.0.0.2"])]);
        e.update_dns_servers(vec!["2606:4700:4700::1111".parse().unwrap()]);
        for up in [false, true, false] {
            state(&e, "10.0.0.2", 2, up);
            assert_eq!(
                e.resolve(&"2606:4700:4700::1111".parse().unwrap(), 0),
                VpnDecision::Block
            );
        }
    }
    #[test]
    fn system_dns_refresh_preserves_forwarder_guards_and_source_removal() {
        let e = engine(vec![rule(
            "203.0.113.0/24",
            VpnRuleAction::Exit,
            &["10.0.0.2"],
        )]);
        let forwarded: IpAddr = "8.8.8.8".parse().unwrap();
        let system: IpAddr = "9.9.9.9".parse().unwrap();
        e.update_dns_servers(vec![forwarded]);
        e.update_route_context(Default::default(), vec![system]);
        assert_eq!(e.resolve(&forwarded, 0), VpnDecision::Block);
        assert_eq!(e.resolve(&system, 0), VpnDecision::Block);
        let generation = e.route_plan().1;
        e.update_system_dns_servers(vec![system, forwarded]);
        assert_eq!(e.route_plan().1, generation);
        e.update_dns_servers(vec![]);
        assert_eq!(e.resolve(&forwarded, 0), VpnDecision::Block);
        e.update_system_dns_servers(vec![]);
        assert_eq!(e.resolve(&forwarded, 0), VpnDecision::Passthrough);
        assert_eq!(e.resolve(&system, 0), VpnDecision::Passthrough);
    }
}
