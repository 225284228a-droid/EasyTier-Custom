//! Host-side VPN route manager.
//!
//! Owns the client-side system routing table while the multi-exit policy VPN
//! is enabled:
//!
//! * exit and block rule CIDRs are routed into the EasyTier TUN (global
//!   default routes are installed as the classic `0.0.0.0/1` +
//!   `128.0.0.0/1` split, and `::/1` + `8000::/1` for IPv6, so the physical
//!   default route never needs to be replaced);
//! * direct rule CIDRs are routed through the captured pre-VPN physical
//!   default gateway;
//! * DNS servers are pinned into the tunnel while a global IPv4 exit is
//!   active (DNS leak protection);
//! * non-mesh IPv6 is blackholed through the TUN when no global IPv6 exit
//!   exists (IPv6 leak protection);
//! * EasyTier's own underlay endpoint addresses are pinned through the
//!   physical gateway so the tunnel cannot route through itself;
//! * under fail-mode direct, Exit-rule routes are held only while one of
//!   the rule's exits is reachable (liveness is pushed by the peer manager
//!   through `VpnExitLivenessUpdated`), so dead exits fail open onto the
//!   physical network instead of blackholing inside the tunnel;
//! * every route this manager installed is removed on disable and on
//!   shutdown.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use easytier_core::config::vpn::{VpnCidr, VpnConfig, VpnFailMode, VpnRuleAction, VpnRuleConfig};
use easytier_core::peers::vpn_policy::{VpnHostRouteState, VpnRouteStateHost};

use crate::common::config::ConfigLoader as _;
use crate::common::global_ctx::{ArcGlobalCtx, GlobalCtxEvent};

type BoxedIfCfg = Arc<dyn crate::common::ifcfg::IfConfiguerTrait>;

/// One route this manager owns.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum VpnRoute {
    /// On-link route through the EasyTier TUN.
    Tun {
        destination: IpAddr,
        prefix: u8,
    },
    /// Route through the captured pre-VPN physical gateway.
    ViaGateway {
        gateway: IpAddr,
        destination: IpAddr,
        prefix: u8,
    },
}

/// Shared status cell installed on the GlobalCtx, read by the VPN status RPC.
#[derive(Clone, Default)]
pub struct VpnRouteStatusCell {
    inner: Arc<Mutex<VpnHostRouteState>>,
}

impl VpnRouteStatusCell {
    pub fn new() -> Self {
        Self::default()
    }

    fn update(&self, state: VpnHostRouteState) {
        *self.inner.lock().unwrap() = state;
    }
}

#[async_trait]
impl VpnRouteStateHost for VpnRouteStatusCell {
    fn vpn_route_state(&self) -> Option<VpnHostRouteState> {
        let state = self.inner.lock().unwrap().clone();
        (state.routes_active || state.routes_installed > 0).then_some(state)
    }
}

#[derive(Default)]
struct VpnRouteManagerState {
    installed: BTreeSet<VpnRoute>,
    captured_gateway_v4: Option<IpAddr>,
    captured_gateway_v6: Option<IpAddr>,
    endpoints: BTreeSet<IpAddr>,
    /// Latest per-exit liveness view (exit name -> reachable), fed by the
    /// peer manager through `GlobalCtxEvent::VpnExitLivenessUpdated`.
    exit_up: BTreeMap<String, bool>,
}

/// Spawns the route-manager task. It runs until the GlobalCtx event bus
/// closes (instance shutdown) and always removes its routes before exiting.
pub async fn run_vpn_route_manager(
    global_ctx: ArcGlobalCtx,
    ifcfg: BoxedIfCfg,
    ifname: String,
    net_ns: crate::common::netns::NetNS,
    status: VpnRouteStatusCell,
) {
    let mut event_receiver = global_ctx.subscribe();
    let mut state = VpnRouteManagerState::default();

    seed_endpoints_from_config(&global_ctx, &mut state).await;
    reconcile(&global_ctx, &ifcfg, &ifname, &net_ns, &status, &mut state).await;

    loop {
        let trigger = tokio::select! {
            event = event_receiver.recv() => {
                match event {
                    Ok(event) => Some(event),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        event_receiver = event_receiver.resubscribe();
                        None
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => None,
        };

        let mut changed = trigger.is_none();
        if let Some(event) = trigger {
            match &event {
                GlobalCtxEvent::TunDeviceReady(_)
                | GlobalCtxEvent::DhcpIpv4Changed(_, _)
                | GlobalCtxEvent::ConfigPatched(_) => {
                    changed = true;
                }
                GlobalCtxEvent::PeerConnAdded(info) => {
                    if let Some(ip) = peer_conn_remote_ip(info) {
                        changed |= state.endpoints.insert(ip);
                    }
                }
                GlobalCtxEvent::PeerConnRemoved(info) => {
                    if let Some(ip) = peer_conn_remote_ip(info) {
                        changed |= state.endpoints.remove(&ip);
                    }
                }
                GlobalCtxEvent::VpnExitLivenessUpdated(liveness) => {
                    let view: BTreeMap<String, bool> = liveness.iter().cloned().collect();
                    if view != state.exit_up {
                        state.exit_up = view;
                        changed = true;
                    }
                }
                _ => {}
            }
        }

        if changed {
            reconcile(&global_ctx, &ifcfg, &ifname, &net_ns, &status, &mut state).await;
        }
    }

    // Shutdown: remove everything we installed so the system routing table is
    // restored. Routes pointing at a destroyed TUN are dropped by the kernel,
    // but gateway routes need explicit removal.
    tracing::info!(
        count = state.installed.len(),
        "vpn route manager shutting down, restoring system routes"
    );
    for route in state.installed.clone() {
        remove_route(&ifcfg, &net_ns, &ifname, &route).await;
    }
    state.installed.clear();
    status.update(VpnHostRouteState::default());
}

fn peer_conn_remote_ip(info: &easytier_proto::api::instance::PeerConnInfo) -> Option<IpAddr> {
    let url: url::Url = info.tunnel.as_ref()?.remote_addr.clone()?.into();
    let ip = match url.host()? {
        url::Host::Ipv4(v4) => IpAddr::V4(v4),
        url::Host::Ipv6(v6) => IpAddr::V6(v6),
        url::Host::Domain(host) => host.parse().ok()?,
    };
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// The host of a configured peer URL when it is a literal IP address.
/// Non-special schemes (tcp://, udp://, ...) keep literal IPv4 hosts as
/// opaque domains, so domains are tried as address literals too.
fn peer_url_host_ip(uri: &url::Url) -> Option<IpAddr> {
    let ip = match uri.host()? {
        url::Host::Ipv4(v4) => IpAddr::V4(v4),
        url::Host::Ipv6(v6) => IpAddr::V6(v6),
        url::Host::Domain(host) => host.parse().ok()?,
    };
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// Best-effort single resolution of a configured peer domain, so its
/// underlay IP can be pinned before the first connection exists.
async fn resolve_peer_domain(domain: &str) -> Option<IpAddr> {
    match tokio::net::lookup_host((domain, 0)).await {
        Ok(addrs) => addrs
            .map(|addr| addr.ip())
            .find(|ip| !ip.is_loopback() && !ip.is_unspecified()),
        Err(err) => {
            tracing::debug!(
                domain,
                ?err,
                "could not resolve configured peer domain for vpn endpoint pin"
            );
            None
        }
    }
}

/// Pre-seeds the underlay endpoints of the configured peers. The
/// PeerConnAdded learning path only fires after a connection exists, so
/// without seeding the /1 takeover routes could capture the very
/// connections EasyTier needs to build the mesh.
async fn seed_endpoints_from_config(
    global_ctx: &ArcGlobalCtx,
    state: &mut VpnRouteManagerState,
) {
    for peer in global_ctx.config.get_peers() {
        let host = match peer.uri.host() {
            Some(url::Host::Domain(domain)) if domain.parse::<IpAddr>().is_err() => {
                resolve_peer_domain(domain).await
            }
            _ => peer_url_host_ip(&peer.uri),
        };
        if let Some(ip) = host
            && state.endpoints.insert(ip)
        {
            tracing::debug!(%ip, url = %peer.uri, "seeded vpn endpoint pin from configured peer");
        }
    }
}

async fn reconcile(
    global_ctx: &ArcGlobalCtx,
    ifcfg: &BoxedIfCfg,
    ifname: &str,
    net_ns: &crate::common::netns::NetNS,
    status: &VpnRouteStatusCell,
    state: &mut VpnRouteManagerState,
) {
    let config: VpnConfig = global_ctx.config.get_vpn();
    let mut desired: BTreeSet<VpnRoute> = BTreeSet::new();
    let mut dns_servers = Vec::new();

    if config.enabled {
        // Refresh the physical gateways on every reconcile. default_route
        // scans the routing table with the TUN excluded, so the physical
        // default stays visible after the /1 takeover routes are installed;
        // when the host switches networks, the direct-rule and endpoint-pin
        // routes follow the new gateway on the next diff. A family with no
        // result keeps its previous capture (a stale pin beats none while
        // offline).
        for (ipv6, slot) in [
            (false, &mut state.captured_gateway_v4),
            (true, &mut state.captured_gateway_v6),
        ] {
            if let Some(gateway) = ifcfg
                .default_route(ipv6, Some(ifname))
                .await
                .ok()
                .flatten()
                .and_then(|route| usable_gateway(route.gateway))
                && slot.is_none_or(|old| old != gateway)
            {
                tracing::info!(
                    family = if ipv6 { "v6" } else { "v4" },
                    ?gateway,
                    "physical default gateway for vpn updated"
                );
                *slot = Some(gateway);
            }
        }

        for rule in &config.rules {
            match rule.action {
                VpnRuleAction::Exit | VpnRuleAction::Block => {
                    if rule_routable(rule, &config, &state.exit_up) {
                        add_tun_routes(&mut desired, rule.cidr);
                    } else {
                        tracing::info!(
                            cidr = %rule.cidr,
                            "vpn rule exits all unreachable; fail-mode direct leaves destinations on the physical network"
                        );
                    }
                }
                VpnRuleAction::Direct => {
                    let gateway = match rule.cidr {
                        VpnCidr::V4(_) => state.captured_gateway_v4,
                        VpnCidr::V6(_) => state.captured_gateway_v6,
                    };
                    match gateway {
                        Some(gateway) => {
                            desired.insert(VpnRoute::ViaGateway {
                                gateway,
                                destination: cidr_first_address(rule.cidr),
                                prefix: rule.cidr.prefix_len(),
                            });
                        }
                        None => {
                            tracing::warn!(
                                cidr = %rule.cidr,
                                "vpn direct rule skipped: no physical gateway captured"
                            );
                        }
                    }
                }
            }
        }

        // DNS leak protection: pin system DNS servers into the tunnel while a
        // global exit covers their family, so queries cannot bypass the exit.
        dns_servers = list_system_dns_servers();
        if config.dns_protect && global_exit_available(&config, true, &state.exit_up) {
            for dns in &dns_servers {
                let covered = match dns {
                    IpAddr::V4(_) => global_exit_available(&config, true, &state.exit_up),
                    IpAddr::V6(_) => global_exit_available(&config, false, &state.exit_up),
                };
                if covered {
                    desired.insert(VpnRoute::Tun {
                        destination: *dns,
                        prefix: host_prefix(dns),
                    });
                }
            }
        }

        // IPv6 leak protection: blackhole non-mesh IPv6 when a global IPv4
        // exit is active but no global IPv6 exit exists.
        // Leak protection trumps fail-open: while a global v4 exit is
        // reachable, v6 stays blackholed unless a global v6 exit is also
        // reachable (not merely configured), so v6 cannot slip around the
        // v4 tunnel when the v6 exit is down.
        if config.ipv6_protect
            && global_exit_available(&config, true, &state.exit_up)
            && !global_exit_available(&config, false, &state.exit_up)
        {
            desired.insert(VpnRoute::Tun {
                destination: IpAddr::V6(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0)),
                prefix: 3,
            });
        }

        // Endpoint protection: keep EasyTier's own underlay connections on
        // the physical network so the tunnel cannot route through itself.
        for endpoint in state.endpoints.iter() {
            let gateway = match endpoint {
                IpAddr::V4(_) => state.captured_gateway_v4,
                IpAddr::V6(_) => state.captured_gateway_v6,
            };
            if let Some(gateway) = gateway {
                desired.insert(VpnRoute::ViaGateway {
                    gateway,
                    destination: *endpoint,
                    prefix: host_prefix(endpoint),
                });
            }
        }
    } else {
        state.captured_gateway_v4 = None;
        state.captured_gateway_v6 = None;
    }

    let to_remove = state
        .installed
        .difference(&desired)
        .cloned()
        .collect::<Vec<_>>();
    let to_add = desired.difference(&state.installed).cloned().collect::<Vec<_>>();
    let mut failures = 0usize;

    for route in to_remove {
        if remove_route(ifcfg, net_ns, ifname, &route).await {
            state.installed.remove(&route);
        }
    }
    for route in to_add {
        if add_route(ifcfg, net_ns, ifname, &route).await {
            state.installed.insert(route);
        } else {
            failures += 1;
        }
    }

    status.update(VpnHostRouteState {
        routes_active: config.enabled,
        routes_installed: state.installed.len(),
        protected_endpoints: state
            .endpoints
            .iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>(),
        system_dns_servers: if config.dns_protect
            && global_exit_available(&config, true, &state.exit_up)
        {
            dns_servers.clone()
        } else {
            Vec::new()
        },
    });

    if failures > 0 {
        tracing::warn!(count = failures, "some vpn routes could not be installed");
    }
    tracing::debug!(
        installed = state.installed.len(),
        active = config.enabled,
        "vpn route reconcile finished"
    );
}

fn usable_gateway(gateway: IpAddr) -> Option<IpAddr> {
    (!gateway.is_unspecified() && !gateway.is_loopback()).then_some(gateway)
}

/// Whether any of the rule's exits (primary then fallbacks) is currently
/// reachable. Exits missing from the liveness view count as down.
fn rule_exit_available(rule: &VpnRuleConfig, exit_up: &BTreeMap<String, bool>) -> bool {
    rule.exit
        .iter()
        .chain(rule.fallback_exits.iter())
        .any(|name| exit_up.get(name).copied().unwrap_or(false))
}

/// Whether the rule's routes may hold the destination right now. Block and
/// Direct rules always apply; Exit rules apply under fail-mode block, and
/// under fail-mode direct only while one of their exits is reachable (the
/// takeover route is withdrawn otherwise, so traffic fails open onto the
/// physical network instead of blackholing inside the tunnel).
fn rule_routable(
    rule: &VpnRuleConfig,
    config: &VpnConfig,
    exit_up: &BTreeMap<String, bool>,
) -> bool {
    match rule.action {
        VpnRuleAction::Exit => match config.fail_mode {
            VpnFailMode::Block => true,
            VpnFailMode::Direct => rule_exit_available(rule, exit_up),
        },
        VpnRuleAction::Direct | VpnRuleAction::Block => true,
    }
}

/// Whether a global (default-route) exit rule of the given family is both
/// configured and currently routable.
fn global_exit_available(
    config: &VpnConfig,
    ipv4: bool,
    exit_up: &BTreeMap<String, bool>,
) -> bool {
    config.enabled
        && config.rules.iter().any(|rule| {
            rule.action == VpnRuleAction::Exit
                && rule.cidr.is_default_route()
                && rule.cidr.is_ipv4() == ipv4
                && rule_routable(rule, config, exit_up)
        })
}

fn host_prefix(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    }
}

/// Adds TUN routes for one rule CIDR, splitting default routes into the /1
/// pair so the physical default route is never replaced.
fn add_tun_routes(desired: &mut BTreeSet<VpnRoute>, cidr: VpnCidr) {
    match cidr {
        VpnCidr::V4(cidr) => {
            if cidr.network_length() == 0 {
                desired.insert(VpnRoute::Tun {
                    destination: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    prefix: 1,
                });
                desired.insert(VpnRoute::Tun {
                    destination: IpAddr::V4(Ipv4Addr::new(128, 0, 0, 0)),
                    prefix: 1,
                });
            } else {
                desired.insert(VpnRoute::Tun {
                    destination: IpAddr::V4(cidr.first_address()),
                    prefix: cidr.network_length(),
                });
            }
        }
        VpnCidr::V6(cidr) => {
            if cidr.network_length() == 0 {
                desired.insert(VpnRoute::Tun {
                    destination: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    prefix: 1,
                });
                desired.insert(VpnRoute::Tun {
                    destination: IpAddr::V6(Ipv6Addr::new(0x8000, 0, 0, 0, 0, 0, 0, 0)),
                    prefix: 1,
                });
            } else {
                desired.insert(VpnRoute::Tun {
                    destination: IpAddr::V6(cidr.first_address()),
                    prefix: cidr.network_length(),
                });
            }
        }
    }
}

fn cidr_first_address(cidr: VpnCidr) -> IpAddr {
    match cidr {
        VpnCidr::V4(cidr) => IpAddr::V4(cidr.first_address()),
        VpnCidr::V6(cidr) => IpAddr::V6(cidr.first_address()),
    }
}

async fn add_route(
    ifcfg: &BoxedIfCfg,
    net_ns: &crate::common::netns::NetNS,
    ifname: &str,
    route: &VpnRoute,
) -> bool {
    let _guard = net_ns.guard();
    let result = match route {
        VpnRoute::Tun {
            destination,
            prefix,
        } => match destination {
            IpAddr::V4(addr) => ifcfg.add_ipv4_route(ifname, *addr, *prefix, Some(1)).await,
            IpAddr::V6(addr) => ifcfg.add_ipv6_route(ifname, *addr, *prefix, Some(1)).await,
        },
        VpnRoute::ViaGateway {
            gateway,
            destination,
            prefix,
        } => ifcfg.add_route_via_gateway(*gateway, *destination, *prefix).await,
    };
    match result {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(?route, ?err, "failed to install vpn route");
            false
        }
    }
}

async fn remove_route(
    ifcfg: &BoxedIfCfg,
    net_ns: &crate::common::netns::NetNS,
    ifname: &str,
    route: &VpnRoute,
) -> bool {
    let _guard = net_ns.guard();
    let result = match route {
        VpnRoute::Tun {
            destination,
            prefix,
        } => match destination {
            IpAddr::V4(addr) => ifcfg.remove_ipv4_route(ifname, *addr, *prefix).await,
            IpAddr::V6(addr) => ifcfg.remove_ipv6_route(ifname, *addr, *prefix).await,
        },
        VpnRoute::ViaGateway {
            gateway,
            destination,
            prefix,
        } => {
            ifcfg
                .remove_route_via_gateway(*gateway, *destination, *prefix)
                .await
        }
    };
    match result {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(?route, ?err, "failed to remove vpn route");
            false
        }
    }
}

/// True for addresses that can be pinned through the tunnel meaningfully:
/// public unicast (or at least not loopback/link-local/multicast/mesh-ULA).
fn is_pinnable_dns(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_multicast()
                && !v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            // fc00::/7 covers EasyTier's fd00::/8 virtual mesh range; those
            // are already inside the overlay and must not be re-pinned.
            let is_unique_local = (v6.segments()[0] & 0xfe00) == 0xfc00;
            !v6.is_loopback()
                && !v6.is_multicast()
                && !v6.is_unspecified()
                && !is_unique_local
                && !v6.is_unicast_link_local()
        }
    }
}

/// Lists the system resolver's DNS servers.
fn list_system_dns_servers() -> Vec<IpAddr> {
    let mut servers: BTreeSet<IpAddr> = BTreeSet::new();

    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = std::process::Command::new("ipconfig")
            .arg("/all")
            .output()
        {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                let lowered = line.to_lowercase();
                if !(lowered.contains("dns servers") || lowered.contains("dns 服务器")) {
                    continue;
                }
                if let Some(pos) = line.find(':') {
                    for token in line[pos + 1..].split_whitespace() {
                        if let Ok(ip) = token.trim_end_matches(',').parse::<IpAddr>() {
                            servers.insert(ip);
                        }
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(content) = std::fs::read_to_string("/etc/resolv.conf") {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("nameserver") {
                    if let Ok(ip) = rest.trim().parse::<IpAddr>() {
                        servers.insert(ip);
                    }
                }
            }
        }
    }

    servers.into_iter().filter(is_pinnable_dns).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use easytier_core::config::vpn::VpnExitConfig;

    #[test]
    fn default_routes_split_into_halves() {
        let mut desired = BTreeSet::new();
        add_tun_routes(&mut desired, "0.0.0.0/0".parse().unwrap());
        assert!(desired.contains(&VpnRoute::Tun {
            destination: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            prefix: 1
        }));
        assert!(desired.contains(&VpnRoute::Tun {
            destination: IpAddr::V4(Ipv4Addr::new(128, 0, 0, 0)),
            prefix: 1
        }));

        let mut desired = BTreeSet::new();
        add_tun_routes(&mut desired, "::/0".parse().unwrap());
        assert!(desired.contains(&VpnRoute::Tun {
            destination: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            prefix: 1
        }));
        assert!(desired.contains(&VpnRoute::Tun {
            destination: IpAddr::V6(Ipv6Addr::new(0x8000, 0, 0, 0, 0, 0, 0, 0)),
            prefix: 1
        }));

        let mut desired = BTreeSet::new();
        add_tun_routes(&mut desired, "10.0.0.0/8".parse().unwrap());
        assert_eq!(desired.len(), 1);
    }

    #[test]
    fn dns_filter_keeps_public_servers_only() {
        assert!(is_pinnable_dns(&"8.8.8.8".parse().unwrap()));
        assert!(!is_pinnable_dns(&"127.0.0.1".parse().unwrap()));
        assert!(!is_pinnable_dns(&"169.254.1.1".parse().unwrap()));
        assert!(is_pinnable_dns(&"2001:4860:4860::8888".parse().unwrap()));
        assert!(!is_pinnable_dns(&"fd00::100".parse().unwrap()));
        assert!(!is_pinnable_dns(&"fe80::1".parse().unwrap()));
    }

    fn vpn_config_with_global_exit(fail_mode: VpnFailMode) -> VpnConfig {
        VpnConfig {
            enabled: true,
            fail_mode,
            exits: vec![VpnExitConfig {
                name: "a".to_owned(),
                node: "10.144.144.2".parse().unwrap(),
                note: None,
            }],
            rules: vec![VpnRuleConfig {
                cidr: "0.0.0.0/0".parse().unwrap(),
                action: VpnRuleAction::Exit,
                exit: Some("a".to_owned()),
                fallback_exits: vec![],
                priority: 0,
            }],
            ..Default::default()
        }
    }

    fn exit_up(entries: &[(&str, bool)]) -> BTreeMap<String, bool> {
        entries
            .iter()
            .map(|(name, up)| ((*name).to_owned(), *up))
            .collect()
    }

    #[test]
    fn fail_mode_direct_withdraws_rules_when_exits_down() {
        let config = vpn_config_with_global_exit(VpnFailMode::Direct);
        let rule = &config.rules[0];

        // unknown liveness (startup, before the first refresh) or down:
        // nothing is routable, so no takeover route is installed
        assert!(!rule_routable(rule, &config, &BTreeMap::new()));
        assert!(!rule_routable(rule, &config, &exit_up(&[("a", false)])));
        assert!(!global_exit_available(&config, true, &exit_up(&[("a", false)])));

        // exit reachable again: routes come back
        assert!(rule_routable(rule, &config, &exit_up(&[("a", true)])));
        assert!(global_exit_available(&config, true, &exit_up(&[("a", true)])));
    }

    #[test]
    fn fail_mode_block_keeps_rules_installed() {
        let config = vpn_config_with_global_exit(VpnFailMode::Block);
        let rule = &config.rules[0];
        assert!(rule_routable(rule, &config, &BTreeMap::new()));
        assert!(rule_routable(rule, &config, &exit_up(&[("a", false)])));
        assert!(global_exit_available(&config, true, &exit_up(&[("a", false)])));
    }

    #[test]
    fn fallback_exit_counts_as_available() {
        let mut config = vpn_config_with_global_exit(VpnFailMode::Direct);
        config.exits.push(VpnExitConfig {
            name: "b".to_owned(),
            node: "10.144.144.3".parse().unwrap(),
            note: None,
        });
        config.rules[0].fallback_exits = vec!["b".to_owned()];
        let rule = &config.rules[0];
        assert!(rule_routable(rule, &config, &exit_up(&[("a", false), ("b", true)])));
        assert!(!rule_routable(rule, &config, &exit_up(&[("a", false), ("b", false)])));
    }

    #[test]
    fn v6_blackhole_requires_reachable_v4_global_exit() {
        let mut config = vpn_config_with_global_exit(VpnFailMode::Direct);
        config.rules.push(VpnRuleConfig {
            cidr: "::/0".parse().unwrap(),
            action: VpnRuleAction::Exit,
            exit: Some("a".to_owned()),
            fallback_exits: vec![],
            priority: 0,
        });

        // v4 exit down: neither family is available
        let down = exit_up(&[("a", false)]);
        assert!(!global_exit_available(&config, true, &down));
        assert!(!global_exit_available(&config, false, &down));

        // both up: v4 available, v6 available too (no blackhole)
        let up = exit_up(&[("a", true)]);
        assert!(global_exit_available(&config, true, &up));
        assert!(global_exit_available(&config, false, &up));

        // v6-only availability does not leak: unreachable v6 exit with a
        // reachable v4 exit is still "not available"
        let v6_down = exit_up(&[("a", false)]);
        assert!(!global_exit_available(&config, false, &v6_down));
    }

    #[test]
    fn peer_url_host_ip_extracts_literal_hosts_only() {
        assert_eq!(
            peer_url_host_ip(&"tcp://1.2.3.4:11010".parse().unwrap()),
            Some("1.2.3.4".parse().unwrap())
        );
        assert_eq!(
            peer_url_host_ip(&"udp://[fd00::9]:11010".parse().unwrap()),
            Some("fd00::9".parse().unwrap())
        );
        assert_eq!(
            peer_url_host_ip(&"tcp://public.easytier.cn:11010".parse().unwrap()),
            None
        );
        assert_eq!(peer_url_host_ip(&"tcp://127.0.0.1:11010".parse().unwrap()), None);
    }

    #[test]
    fn status_cell_reports_only_when_active() {
        let cell = VpnRouteStatusCell::new();
        assert!(VpnRouteStateHost::vpn_route_state(&cell).is_none());
        cell.update(VpnHostRouteState {
            routes_active: true,
            routes_installed: 3,
            ..Default::default()
        });
        let state = VpnRouteStateHost::vpn_route_state(&cell).unwrap();
        assert!(state.routes_active);
        assert_eq!(state.routes_installed, 3);
    }
}
