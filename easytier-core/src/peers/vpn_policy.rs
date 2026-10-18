//! Single-exit packet decisions and capture routes independent of exit liveness.
use crate::config::{
    PeerId,
    vpn::{
        VpnConfig,
        routes::{VpnRouteExclusions, VpnRoutePlan, compile_vpn_routes},
    },
};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpnDecision {
    Passthrough,
    Drop,
    Exit(PeerId),
}
struct VpnCompiledState {
    config: VpnConfig,
    exclusions: VpnRouteExclusions,
    plan: VpnRoutePlan,
    generation: u64,
}
pub struct VpnRuleStatusEntry {
    pub cidr: String,
    pub exit_addr: String,
    pub up: bool,
}
pub struct VpnPolicyEngine {
    state: ArcSwap<VpnCompiledState>,
    exit_states: DashMap<IpAddr, Option<PeerId>>,
    update_lock: Mutex<()>,
    config_changes: tokio::sync::watch::Sender<VpnConfig>,
}
impl VpnPolicyEngine {
    pub fn new(config: VpnConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let exclusions = VpnRouteExclusions::default();
        let plan = compile_vpn_routes(&config, &exclusions);
        let (config_changes, _) = tokio::sync::watch::channel(config.clone());
        let engine = Self {
            state: ArcSwap::from_pointee(VpnCompiledState {
                config,
                exclusions,
                plan,
                generation: 1,
            }),
            exit_states: DashMap::new(),
            update_lock: Mutex::new(()),
            config_changes,
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
        let plan = compile_vpn_routes(&config, &old.exclusions);
        self.state.store(Arc::new(VpnCompiledState {
            config: config.clone(),
            exclusions: old.exclusions.clone(),
            plan,
            generation: old.generation.wrapping_add(1),
        }));
        self.sync_exit_state_slots();
        self.config_changes.send_replace(config);
        Ok(())
    }
    /// Subscribe before reading the initial configuration so concurrent updates
    /// cannot be lost. Exclusions and reachability belong to each peer.
    pub(crate) fn subscribe_config_changes(&self) -> tokio::sync::watch::Receiver<VpnConfig> {
        self.config_changes.subscribe()
    }
    #[cfg(test)]
    pub(crate) fn config_subscriber_count(&self) -> usize {
        self.config_changes.receiver_count()
    }
    fn sync_exit_state_slots(&self) {
        let addresses = self.state.load().config.exit_addresses();
        for a in &addresses {
            self.exit_states.entry(*a).or_default();
        }
        self.exit_states.retain(|a, _| addresses.contains(a));
    }
    pub fn update_exit_states(&self, updates: impl IntoIterator<Item = (IpAddr, Option<PeerId>)>) {
        for (address, peer_id) in updates {
            if let Some(mut slot) = self.exit_states.get_mut(&address) {
                *slot = peer_id;
            }
        }
    }
    pub fn config(&self) -> VpnConfig {
        self.state.load().config.clone()
    }
    pub fn route_plan(&self) -> (VpnRoutePlan, u64) {
        let state = self.state.load();
        (state.plan.clone(), state.generation)
    }
    pub fn update_exclusions(&self, exclusions: VpnRouteExclusions) {
        let _guard = self.update_lock.lock().unwrap();
        let old = self.state.load_full();
        if old.exclusions == exclusions {
            return;
        }
        let plan = compile_vpn_routes(&old.config, &exclusions);
        self.state.store(Arc::new(VpnCompiledState {
            config: old.config.clone(),
            exclusions,
            plan,
            generation: old.generation.wrapping_add(1),
        }));
    }
    pub fn resolve(&self, destination: &IpAddr) -> VpnDecision {
        let state = self.state.load();
        if state
            .exclusions
            .physical
            .iter()
            .chain(&state.exclusions.overlay)
            .any(|c| c.contains(destination))
        {
            return VpnDecision::Passthrough;
        }
        let Some(index) = state.config.match_rule_index(destination) else {
            return VpnDecision::Passthrough;
        };
        self.exit_states
            .get(&state.config.rules[index].exit)
            .and_then(|slot| *slot)
            .map(VpnDecision::Exit)
            .unwrap_or(VpnDecision::Drop)
    }
    pub fn status(&self) -> Vec<VpnRuleStatusEntry> {
        self.state
            .load()
            .config
            .rules
            .iter()
            .map(|rule| VpnRuleStatusEntry {
                cidr: rule.cidr.to_string(),
                exit_addr: rule.exit.to_string(),
                up: self
                    .exit_states
                    .get(&rule.exit)
                    .is_some_and(|s| s.is_some()),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VpnHostRouteState {
    pub routes_active: bool,
    pub routes_installed: usize,
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
    fn engine(rules: &[(&str, &str)]) -> VpnPolicyEngine {
        VpnPolicyEngine::new(VpnConfig {
            rules: rules
                .iter()
                .map(|(cidr, exit)| VpnRuleConfig {
                    cidr: cidr.parse().unwrap(),
                    exit: exit.parse().unwrap(),
                })
                .collect(),
            ..Default::default()
        })
        .unwrap()
    }
    fn resolve(engine: &VpnPolicyEngine, ip: &str) -> VpnDecision {
        engine.resolve(&ip.parse().unwrap())
    }
    #[test]
    fn two_subnets_use_their_own_exit_and_ipv6_is_explicit() {
        let e = engine(&[
            ("203.0.113.0/24", "10.0.0.2"),
            ("198.51.100.0/24", "10.0.0.3"),
            ("2001:db8::/32", "fd42::2"),
        ]);
        e.update_exit_states([
            ("10.0.0.2".parse().unwrap(), Some(2)),
            ("10.0.0.3".parse().unwrap(), Some(3)),
            ("fd42::2".parse().unwrap(), Some(4)),
        ]);
        assert_eq!(resolve(&e, "203.0.113.1"), VpnDecision::Exit(2));
        assert_eq!(resolve(&e, "198.51.100.1"), VpnDecision::Exit(3));
        assert_eq!(resolve(&e, "2001:db8::1"), VpnDecision::Exit(4));
        assert_eq!(resolve(&e, "8.8.8.8"), VpnDecision::Passthrough);
        assert_eq!(
            resolve(&e, "2606:4700:4700::1111"),
            VpnDecision::Passthrough
        );
    }
    #[test]
    fn unavailable_specific_exit_never_falls_back_and_does_not_change_routes() {
        let e = engine(&[("0.0.0.0/0", "10.0.0.2"), ("10.0.0.0/8", "10.0.0.3")]);
        let initial = e.route_plan();
        e.update_exit_states([("10.0.0.2".parse().unwrap(), Some(2))]);
        assert_eq!(resolve(&e, "10.1.0.1"), VpnDecision::Drop);
        assert_eq!(resolve(&e, "8.8.8.8"), VpnDecision::Exit(2));
        for peer in [Some(3), None, Some(3)] {
            e.update_exit_states([("10.0.0.3".parse().unwrap(), peer)]);
            assert_eq!(
                resolve(&e, "10.1.0.1"),
                peer.map(VpnDecision::Exit).unwrap_or(VpnDecision::Drop)
            );
            assert_eq!(e.route_plan(), initial);
        }
    }
    #[test]
    fn mesh_and_proxy_exclusions_win_but_other_private_subnets_can_use_exits() {
        let e = engine(&[("0.0.0.0/0", "10.66.0.2"), ("::/0", "10.66.0.2")]);
        e.update_exit_states([("10.66.0.2".parse().unwrap(), Some(2))]);
        e.update_exclusions(VpnRouteExclusions {
            overlay: vec![
                "10.66.0.0/24".parse().unwrap(),
                "203.0.113.0/24".parse().unwrap(),
                "fd42::/64".parse().unwrap(),
            ],
            physical: vec!["192.168.1.0/24".parse().unwrap()],
        });
        for ip in ["10.66.0.2", "203.0.113.2", "fd42::2", "192.168.1.2"] {
            assert_eq!(resolve(&e, ip), VpnDecision::Passthrough);
        }
        for ip in ["172.20.0.1", "fd99::1"] {
            assert_eq!(resolve(&e, ip), VpnDecision::Exit(2));
        }
    }
    #[test]
    fn hot_updates_preserve_reachability_and_clear_capture() {
        let e = engine(&[("203.0.113.0/24", "10.0.0.2")]);
        e.update_exit_states([("10.0.0.2".parse().unwrap(), Some(2))]);
        let mut config = e.config();
        config.rules[0].cidr = "198.51.100.0/24".parse().unwrap();
        let generation = e.route_plan().1;
        e.install(config.clone()).unwrap();
        assert!(e.route_plan().1 > generation);
        assert_eq!(resolve(&e, "198.51.100.1"), VpnDecision::Exit(2));
        assert!(e.status()[0].up);
        config.rules.clear();
        e.install(config).unwrap();
        assert!(e.route_plan().0.exit_routes.is_empty());
        assert!(e.status().is_empty());
        assert_eq!(resolve(&e, "198.51.100.1"), VpnDecision::Passthrough);
    }

    #[tokio::test]
    async fn config_subscription_retains_latest_update_and_ignores_local_state() {
        let e = engine(&[("203.0.113.0/24", "10.0.0.2")]);
        let mut changes = e.subscribe_config_changes();
        let mut config = e.config();
        config.rules[0].cidr = "198.51.100.0/24".parse().unwrap();
        e.install(config.clone()).unwrap();
        config.rules[0].exit = "10.0.0.3".parse().unwrap();
        e.install(config.clone()).unwrap();
        changes.changed().await.unwrap();
        assert_eq!(*changes.borrow_and_update(), config);
        e.install(config.clone()).unwrap();
        e.update_exit_states([("10.0.0.3".parse().unwrap(), Some(3))]);
        e.update_exclusions(VpnRouteExclusions {
            physical: vec!["192.168.1.0/24".parse().unwrap()],
            ..Default::default()
        });
        let mut invalid = config.clone();
        invalid.rules.push(invalid.rules[0].clone());
        assert!(e.install(invalid).is_err());
        assert!(!changes.has_changed().unwrap());
        // Subscribers created after an update still see its latest configuration.
        assert_eq!(*e.subscribe_config_changes().borrow(), config);
    }
}
