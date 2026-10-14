use crate::{
    common::{
        config::ConfigLoader as _, global_ctx::ArcGlobalCtx, ifcfg::IfConfiguerTrait, netns::NetNS,
    },
    socket_protector::{self, DesktopProtectionLease},
};
use async_trait::async_trait;
use easytier_core::{
    config::vpn::{VpnCidr, VpnRuleAction},
    instance::CorePacketPlane,
    peers::vpn_policy::{VpnHostRouteState, VpnRouteStateHost},
};
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

type BoxedIfCfg = Arc<dyn IfConfiguerTrait>;

pub struct VpnRouteManagerContext {
    pub global_ctx: ArcGlobalCtx,
    pub packet_plane: Arc<CorePacketPlane>,
    pub ifcfg: Arc<dyn IfConfiguerTrait>,
    pub ifname: String,
    pub net_ns: NetNS,
    pub status: VpnRouteStatusCell,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Default)]
pub struct VpnRouteStatusCell {
    inner: Arc<Mutex<VpnHostRouteState>>,
}
impl VpnRouteStatusCell {
    pub fn new() -> Self {
        Self::default()
    }
    pub(crate) fn update(&self, state: VpnHostRouteState) {
        *self.inner.lock().unwrap() = state;
    }
}
#[async_trait]
impl VpnRouteStateHost for VpnRouteStatusCell {
    fn vpn_route_state(&self) -> Option<VpnHostRouteState> {
        Some(self.inner.lock().unwrap().clone())
    }
}

#[derive(Default)]
struct RouteOwner {
    installed: BTreeSet<VpnCidr>,
    protection: Option<DesktopProtectionLease>,
    #[cfg(target_os = "linux")]
    linux: Option<LinuxPolicyTable>,
    #[cfg(target_os = "linux")]
    pending: BTreeSet<VpnCidr>,
    #[cfg(target_os = "linux")]
    local_overlay: BTreeSet<VpnCidr>,
}

/// Holds the capture set until policy changes or explicit cancellation. Exit
/// liveness never withdraws a route: a rule whose exits are all down blocks.
pub async fn run_vpn_route_manager(
    context: VpnRouteManagerContext,
    ready: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    let VpnRouteManagerContext {
        global_ctx,
        packet_plane,
        ifcfg,
        ifname,
        net_ns,
        status,
        cancellation,
    } = context;
    let mut events = global_ctx.subscribe();
    let mut owner = RouteOwner::default();
    let mut ready = Some(ready);
    let mut startup_error = None;
    // Crash recovery: a previous process that died between activation and
    // cleanup leaves ip rules, OUTPUT guard chains and dedicated-table routes
    // in the kernel. The residue keeps draining traffic toward a retired TUN
    // and permanently blocks the next activation ("no two adjacent safe VPN
    // priorities"), so reclaim it before the first reconcile.
    #[cfg(target_os = "linux")]
    match socket_protector::namespace_for(&net_ns) {
        Ok(namespace) => {
            if let Some(dir) = policy_lease_dir() {
                sweep_stale_policy_state(&net_ns, namespace, &dir).await;
            }
        }
        Err(error) => tracing::warn!(
            ?error,
            "VPN namespace discovery failed; skipping stale policy state sweep"
        ),
    }
    loop {
        let config = global_ctx.config.get_vpn();
        let dns = super::list_system_dns_servers(&net_ns);
        let (plan, _) = packet_plane.vpn_route_plan(dns.clone()).await;
        // Overlay routes have their existing NIC/proxy owners. Installing
        // duplicate VPN routes would erase those owners during VPN disable.
        #[cfg(not(target_os = "linux"))]
        let desired = plan
            .regions
            .iter()
            .filter(|region| matches!(region.action, VpnRuleAction::Exit | VpnRuleAction::Block))
            .flat_map(|region| split_default(region.cidr))
            .collect::<BTreeSet<_>>();
        #[cfg(target_os = "linux")]
        let desired = if config.client_active() {
            plan.capture_routes
                .iter()
                .copied()
                .flat_map(split_default)
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        #[cfg(target_os = "linux")]
        {
            owner.local_overlay = linux_local_overlay(&plan);
        }
        let result = owner
            .reconcile(
                &ifcfg,
                &net_ns,
                &ifname,
                &desired,
                global_ctx.get_flags().socket_mark.unwrap_or(0),
            )
            .await;
        if let Err(error) = &result {
            tracing::error!(
                ?error,
                "VPN capture activation failed; native protection or route setup is unavailable"
            );
        }
        status.update(VpnHostRouteState {
            routes_active: result.is_ok()
                && !desired.is_empty()
                && owner.installed == desired
                && owner.active(),
            routes_installed: owner.installed.len(),
            protected_endpoints: Vec::new(),
            activation_error: result.as_ref().err().map(ToString::to_string),
            system_dns_servers: if config.client_active() && config.dns_protect {
                dns
            } else {
                Vec::new()
            },
        });
        if let Some(ready) = ready.take() {
            let failed = result.is_err();
            startup_error = result.as_ref().err().map(ToString::to_string);
            let _ = ready.send(result.map_err(|error| error.to_string()));
            if failed {
                break;
            }
        }
        tokio::select! {
            _ = cancellation.cancelled() => break,
            event = events.recv() => {
                match event {
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => events = events.resubscribe(),
                    _ => {}
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
        }
    }
    let cleanup = owner
        .reconcile(&ifcfg, &net_ns, &ifname, &BTreeSet::new(), 0)
        .await;
    if let Err(error) = &cleanup {
        tracing::error!(
            ?error,
            "VPN route cleanup failed during awaited NIC shutdown"
        );
    }
    status.update(VpnHostRouteState {
        routes_installed: owner.installed.len(),
        activation_error: cleanup
            .err()
            .map(|error| error.to_string())
            .or(startup_error),
        ..Default::default()
    });
}

fn split_default(cidr: VpnCidr) -> Vec<VpnCidr> {
    if !cidr.is_default_route() {
        return vec![cidr];
    }
    if cidr.is_ipv4() {
        vec!["0.0.0.0/1".parse().unwrap(), "128.0.0.0/1".parse().unwrap()]
    } else {
        vec!["::/1".parse().unwrap(), "8000::/1".parse().unwrap()]
    }
}

impl RouteOwner {
    fn active(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.protection.is_some()
            && self.linux.as_ref().is_some_and(|table| {
                table.active.iter().all(|active| *active)
                    && table.blackhole_active.iter().all(|active| *active)
                    && table
                        .output_guard
                        .ready_for(&linux_output_rules(&self.installed, &self.local_overlay))
            });
        #[cfg(not(target_os = "linux"))]
        self.protection.is_some()
    }

    async fn reconcile(
        &mut self,
        ifcfg: &BoxedIfCfg,
        net_ns: &NetNS,
        ifname: &str,
        desired: &BTreeSet<VpnCidr>,
        configured_mark: u32,
    ) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        let _allocation_guard = linux_allocation_gate().lock().await;
        #[cfg(target_os = "linux")]
        let output_rules = linux_output_rules(desired, &self.local_overlay);
        #[cfg(not(target_os = "linux"))]
        let mut defaults = [None, None];
        #[cfg(not(target_os = "linux"))]
        if !desired.is_empty() {
            for (family, index) in [(false, 0), (true, 1)] {
                defaults[index] = ifcfg
                    .default_route(family, Some(ifname))
                    .await?
                    .and_then(|route| socket_protector::interface_index(&route.interface_name));
            }
            if let Some(protection) = self.protection.as_mut() {
                protection.refresh_defaults(defaults)?;
            }
        }
        if !desired.is_empty() && self.protection.is_none() {
            anyhow::ensure!(
                cfg!(feature = "dns-resolver"),
                "VPN capture requires protected bootstrap DNS support (dns-resolver feature)"
            );
            let namespace = socket_protector::namespace_for(net_ns)?;
            #[cfg(target_os = "linux")]
            {
                let table = LinuxPolicyTable::prepare(net_ns, namespace, configured_mark).await?;
                let protection =
                    DesktopProtectionLease::acquire(namespace, table.bit, [None, None])?;
                self.protection = Some(protection);
                self.linux = Some(table);
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = configured_mark;
                if !socket_protector::native_socket_protection_available() {
                    anyhow::bail!("native VPN socket protection is unsupported on this platform");
                }
                self.protection = Some(DesktopProtectionLease::acquire(namespace, 0, defaults)?);
            }
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if !desired.is_empty() {
            let mut interfaces =
                crate::common::network::collect_interfaces(net_ns.clone(), true).await;
            interfaces.extend(
                crate::common::network::collect_interfaces(net_ns.clone(), false)
                    .await
                    .into_iter()
                    .filter(|interface| interface.internal),
            );
            interfaces.retain(|interface| interface.name != ifname);
            if let Some(protection) = self.protection.as_mut() {
                protection.refresh_interfaces(interfaces);
            }
        }
        #[cfg(target_os = "linux")]
        if !desired.is_empty() {
            self.linux
                .as_mut()
                .expect("protection acquired before output guard")
                .output_guard
                .add_desired(net_ns, ifname, &output_rules)
                .await?;
        }
        // Add captures first. Failed additions leave the old capture set in
        // place; they must never withdraw a dead Exit region onto the host.
        let mut to_add = desired
            .difference(&self.installed)
            .copied()
            .collect::<Vec<_>>();
        #[cfg(target_os = "linux")]
        to_add.extend(self.pending.intersection(desired).copied());
        to_add.sort();
        to_add.dedup();
        for cidr in to_add {
            if let Err(error) = self.add(ifcfg, net_ns, ifname, cidr).await {
                #[cfg(target_os = "linux")]
                self.pending.insert(cidr);
                return Err(error);
            }
            self.installed.insert(cidr);
            #[cfg(target_os = "linux")]
            self.pending.remove(&cidr);
        }
        #[cfg(target_os = "linux")]
        if let Some(table) = self.linux.as_mut()
            && !desired.is_empty()
        {
            table.activate(net_ns).await?;
        }
        let mut to_remove = self
            .installed
            .difference(desired)
            .copied()
            .collect::<Vec<_>>();
        #[cfg(target_os = "linux")]
        to_remove.extend(self.pending.difference(desired).copied());
        to_remove.sort();
        to_remove.dedup();
        for cidr in to_remove {
            self.remove(ifcfg, net_ns, ifname, cidr).await?;
            self.installed.remove(&cidr);
            #[cfg(target_os = "linux")]
            self.pending.remove(&cidr);
        }
        #[cfg(target_os = "linux")]
        if !desired.is_empty() {
            self.linux
                .as_mut()
                .expect("route table owner exists")
                .output_guard
                .remove_obsolete(net_ns, &output_rules)
                .await?;
        }
        if desired.is_empty() {
            #[cfg(target_os = "linux")]
            if let Some(table) = self.linux.as_mut() {
                table.release(net_ns).await?;
                table.output_guard.release(net_ns).await?;
            }
            #[cfg(target_os = "linux")]
            {
                self.linux.take();
            }
            if let Some(protection) = self.protection.as_mut() {
                protection.release()?;
            }
            self.protection.take();
        }
        Ok(())
    }

    async fn add(
        &self,
        ifcfg: &BoxedIfCfg,
        net_ns: &NetNS,
        ifname: &str,
        cidr: VpnCidr,
    ) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        let _ = ifcfg;
        #[cfg(target_os = "linux")]
        {
            let table = self
                .linux
                .as_ref()
                .expect("protection acquired before routes");
            let destination = cidr.to_string();
            let capture_table = table.table.to_string();
            let blackhole_table = table.blackhole_table.to_string();
            ip(
                net_ns,
                &[
                    family(cidr),
                    "route",
                    "replace",
                    "unreachable",
                    &destination,
                    "table",
                    &blackhole_table,
                    "proto",
                    "static",
                ],
            )
            .await?;
            ip(
                net_ns,
                &[
                    family(cidr),
                    "route",
                    "replace",
                    &destination,
                    "dev",
                    ifname,
                    "table",
                    &capture_table,
                    "proto",
                    "static",
                ],
            )
            .await
            .map(|_| ())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _guard = net_ns.guard();
            match cidr {
                VpnCidr::V4(cidr) => {
                    ifcfg
                        .add_ipv4_route(
                            ifname,
                            cidr.first_address(),
                            cidr.network_length(),
                            Some(1),
                        )
                        .await?
                }
                VpnCidr::V6(cidr) => {
                    ifcfg
                        .add_ipv6_route(
                            ifname,
                            cidr.first_address(),
                            cidr.network_length(),
                            Some(1),
                        )
                        .await?
                }
            }
            Ok(())
        }
    }

    async fn remove(
        &self,
        ifcfg: &BoxedIfCfg,
        net_ns: &NetNS,
        ifname: &str,
        cidr: VpnCidr,
    ) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        let _ = ifcfg;
        #[cfg(target_os = "linux")]
        {
            let table = self.linux.as_ref().expect("route table owner exists");
            let family = family(cidr);
            let destination = cidr.to_string();
            let capture_table = table.table.to_string();
            let blackhole_table = table.blackhole_table.to_string();
            let capture_routes: Vec<serde_json::Value> = serde_json::from_slice(
                &ip(
                    net_ns,
                    &[family, "-j", "route", "show", "table", &capture_table],
                )
                .await?,
            )?;
            if linux_route_exists(&capture_routes, &destination, "unicast", Some(ifname), None) {
                ip(
                    net_ns,
                    &[
                        family,
                        "route",
                        "del",
                        &destination,
                        "dev",
                        ifname,
                        "table",
                        &capture_table,
                        "proto",
                        "static",
                    ],
                )
                .await?;
            }
            let blackhole_routes: Vec<serde_json::Value> = serde_json::from_slice(
                &ip(
                    net_ns,
                    &[family, "-j", "route", "show", "table", &blackhole_table],
                )
                .await?,
            )?;
            if linux_route_exists(&blackhole_routes, &destination, "unreachable", None, None) {
                ip(
                    net_ns,
                    &[
                        family,
                        "route",
                        "del",
                        "unreachable",
                        &destination,
                        "table",
                        &blackhole_table,
                        "proto",
                        "static",
                    ],
                )
                .await?;
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _guard = net_ns.guard();
            match cidr {
                VpnCidr::V4(cidr) => {
                    ifcfg
                        .remove_ipv4_route(ifname, cidr.first_address(), cidr.network_length())
                        .await?
                }
                VpnCidr::V6(cidr) => {
                    ifcfg
                        .remove_ipv6_route(ifname, cidr.first_address(), cidr.network_length())
                        .await?
                }
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn family(cidr: VpnCidr) -> &'static str {
    if cidr.is_ipv4() { "-4" } else { "-6" }
}

#[cfg(target_os = "linux")]
async fn ip(net_ns: &NetNS, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    // Spawn within the namespace, then restore it before awaiting the child.
    // Never keep a thread-local setns guard over a Tokio suspension point.
    let child = net_ns.run(|| {
        tokio::process::Command::new("ip")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
    })?;
    let output = child.wait_with_output().await?;
    anyhow::ensure!(
        output.status.success(),
        "ip {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

#[cfg(target_os = "linux")]
struct LinuxPolicyTable {
    namespace: u64,
    table: u32,
    blackhole_table: u32,
    priority: [u32; 2],
    blackhole_priority: [u32; 2],
    bit: u32,
    active: [bool; 2],
    blackhole_active: [bool; 2],
    output_guard: LinuxOutputGuard,
}

/// SO_BINDTODEVICE can bypass the normal RPDB input-interface selectors and
/// IPv6 can skip reject-table routes when an output interface is forced. An
/// OUTPUT-only guard therefore enforces capture at the actual packet hook.
/// Forwarded TUN traffic never traverses this hook; protected native sockets
/// carry the reserved mark bit and skip every guard rule.
#[cfg(target_os = "linux")]
struct LinuxOutputGuard {
    chain: String,
    comment: String,
    mark: String,
    ifname: String,
    created: [bool; 2],
    jumped: [bool; 2],
    installed: BTreeSet<LinuxOutputRule>,
    pending: BTreeSet<LinuxOutputRule>,
    control_installed: BTreeSet<LinuxIpv6ControlRule>,
    control_pending: BTreeSet<LinuxIpv6ControlRule>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LinuxIpv6ControlRule {
    icmp_type: u8,
    hop_limit: u8,
    source: Option<&'static str>,
    destination: Option<&'static str>,
}

#[cfg(target_os = "linux")]
fn linux_ipv6_control_rules() -> BTreeSet<LinuxIpv6ControlRule> {
    let mut rules = BTreeSet::new();
    for icmp_type in [133, 134] {
        for destination in ["fe80::/10", "ff02::/16"] {
            rules.insert(LinuxIpv6ControlRule {
                icmp_type,
                hop_limit: 255,
                source: (icmp_type == 134).then_some("fe80::/10"),
                destination: Some(destination),
            });
        }
    }
    // Neighbor solicitations/advertisements also use global unicast neighbor
    // addresses. Kernel control packets do not inherit the protected socket's
    // mark, so excluding them would prevent a marked socket resolving its peer.
    for icmp_type in [135, 136] {
        rules.insert(LinuxIpv6ControlRule {
            icmp_type,
            hop_limit: 255,
            source: None,
            destination: None,
        });
    }
    rules
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LinuxOutputRule {
    cidr: VpnCidr,
    local_overlay: bool,
}

#[cfg(target_os = "linux")]
fn linux_local_overlay(
    plan: &easytier_core::config::vpn::routes::VpnRoutePlan,
) -> BTreeSet<VpnCidr> {
    // The compiler's capture union contains only overlay and policy captures.
    // Subtract policy regions from that exact snapshot: merged capture CIDRs
    // must not expand the local-delivery exception into Exit or Block regions.
    let policy = plan
        .regions
        .iter()
        .filter(|region| region.action != VpnRuleAction::Direct)
        .map(|region| region.cidr)
        .collect::<Vec<_>>();
    plan.capture_routes
        .iter()
        .flat_map(|cidr| easytier_core::config::vpn::routes::subtract_cidrs(*cidr, &policy))
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_output_rules(
    capture: &BTreeSet<VpnCidr>,
    local_overlay: &BTreeSet<VpnCidr>,
) -> BTreeSet<LinuxOutputRule> {
    let overlay = local_overlay.iter().copied().collect::<Vec<_>>();
    let mut rules = BTreeSet::new();
    for cidr in capture {
        rules.extend(
            easytier_core::config::vpn::routes::subtract_cidrs(*cidr, &overlay)
                .into_iter()
                .map(|cidr| LinuxOutputRule {
                    cidr,
                    local_overlay: false,
                }),
        );
        for overlay_cidr in &overlay {
            if cidr.is_ipv4() != overlay_cidr.is_ipv4() {
                continue;
            }
            let intersection = if cidr.contains(&overlay_cidr.first_address())
                && cidr.prefix_len() <= overlay_cidr.prefix_len()
            {
                Some(*overlay_cidr)
            } else if overlay_cidr.contains(&cidr.first_address())
                && overlay_cidr.prefix_len() <= cidr.prefix_len()
            {
                Some(*cidr)
            } else {
                None
            };
            if let Some(cidr) = intersection {
                rules.insert(LinuxOutputRule {
                    cidr,
                    local_overlay: true,
                });
            }
        }
    }
    rules
}

#[cfg(target_os = "linux")]
impl LinuxOutputGuard {
    fn new(table: u32, bit: u32) -> Self {
        Self {
            chain: format!("ETVPN_{table}"),
            comment: format!("easytier-vpn-{table}"),
            mark: format!("0/{bit:#x}"),
            ifname: String::new(),
            created: [false; 2],
            jumped: [false; 2],
            installed: BTreeSet::new(),
            pending: BTreeSet::new(),
            control_installed: BTreeSet::new(),
            control_pending: BTreeSet::new(),
        }
    }

    fn ready_for(&self, desired: &BTreeSet<LinuxOutputRule>) -> bool {
        self.created.iter().all(|active| *active)
            && self.jumped.iter().all(|active| *active)
            && self.installed == *desired
            && self.pending.is_empty()
            && self.control_installed == linux_ipv6_control_rules()
            && self.control_pending.is_empty()
    }

    fn jump_args(&self, operation: &str) -> Vec<String> {
        let mut args = vec![operation.to_owned(), "OUTPUT".to_owned()];
        if operation == "-I" {
            args.push("1".to_owned());
        }
        args.extend([
            "-m".to_owned(),
            "comment".to_owned(),
            "--comment".to_owned(),
            self.comment.clone(),
            "-j".to_owned(),
            self.chain.clone(),
        ]);
        args
    }

    fn rule_args(&self, operation: &str, rule: LinuxOutputRule) -> Vec<String> {
        let mut args = vec![
            operation.to_owned(),
            self.chain.clone(),
            "-m".to_owned(),
            "mark".to_owned(),
            "--mark".to_owned(),
            self.mark.clone(),
            "-d".to_owned(),
            rule.cidr.to_string(),
            "!".to_owned(),
            "-o".to_owned(),
            self.ifname.clone(),
        ];
        if rule.local_overlay {
            args.extend([
                "-m".to_owned(),
                "addrtype".to_owned(),
                "!".to_owned(),
                "--dst-type".to_owned(),
                "LOCAL".to_owned(),
            ]);
        }
        args.extend(["-j".to_owned(), "DROP".to_owned()]);
        args
    }

    fn control_rule_args(&self, operation: &str, rule: LinuxIpv6ControlRule) -> Vec<String> {
        let mut args = vec![
            operation.to_owned(),
            self.chain.clone(),
            "-m".to_owned(),
            "mark".to_owned(),
            "--mark".to_owned(),
            self.mark.clone(),
            "!".to_owned(),
            "-o".to_owned(),
            self.ifname.clone(),
            "-p".to_owned(),
            "ipv6-icmp".to_owned(),
            "--icmpv6-type".to_owned(),
            format!("{}/0", rule.icmp_type),
            "-m".to_owned(),
            "hl".to_owned(),
            "--hl-eq".to_owned(),
            rule.hop_limit.to_string(),
        ];
        if let Some(source) = rule.source {
            args.extend(["-s".to_owned(), source.to_owned()]);
        }
        if let Some(destination) = rule.destination {
            args.extend(["-d".to_owned(), destination.to_owned()]);
        }
        // RETURN only skips this owner's guard, retaining the host's remaining
        // OUTPUT firewall policy. Ordinary UDP and ICMP Echo never match.
        args.extend(["-j".to_owned(), "RETURN".to_owned()]);
        args
    }

    async fn add_desired(
        &mut self,
        net_ns: &NetNS,
        ifname: &str,
        desired: &BTreeSet<LinuxOutputRule>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.ifname.is_empty() || self.ifname == ifname,
            "VPN output guard interface changed unexpectedly"
        );
        self.ifname = ifname.to_owned();
        for (index, ipv6) in [false, true].into_iter().enumerate() {
            if !self.created[index] {
                // Creation fails on collision. Never flush or reuse another
                // owner's chain, even when its name resembles ours.
                firewall(net_ns, ipv6, &["-N".to_owned(), self.chain.clone()]).await?;
                self.created[index] = true;
            }
        }
        let controls = linux_ipv6_control_rules();
        let mut control_additions = controls
            .difference(&self.control_installed)
            .copied()
            .collect::<BTreeSet<_>>();
        control_additions.extend(self.control_pending.intersection(&controls).copied());
        for rule in control_additions {
            self.control_pending.insert(rule);
            if !firewall_rule_exists(net_ns, true, &self.control_rule_args("-C", rule)).await? {
                firewall(net_ns, true, &self.control_rule_args("-A", rule)).await?;
            }
            self.control_installed.insert(rule);
            self.control_pending.remove(&rule);
        }
        let mut additions = desired
            .difference(&self.installed)
            .copied()
            .collect::<BTreeSet<_>>();
        additions.extend(self.pending.intersection(desired).copied());
        for rule in additions {
            // Track before attempting mutation so a failed/partial setup can
            // query and remove any rule that reached the kernel on cleanup.
            self.pending.insert(rule);
            let ipv6 = !rule.cidr.is_ipv4();
            if !firewall_rule_exists(net_ns, ipv6, &self.rule_args("-C", rule)).await? {
                firewall(net_ns, ipv6, &self.rule_args("-A", rule)).await?;
            }
            self.installed.insert(rule);
            self.pending.remove(&rule);
        }
        for (index, ipv6) in [false, true].into_iter().enumerate() {
            if !self.jumped[index] {
                firewall(net_ns, ipv6, &self.jump_args("-I")).await?;
                self.jumped[index] = true;
            }
        }
        Ok(())
    }

    async fn remove_obsolete(
        &mut self,
        net_ns: &NetNS,
        desired: &BTreeSet<LinuxOutputRule>,
    ) -> anyhow::Result<()> {
        let mut removals = self
            .installed
            .difference(desired)
            .copied()
            .collect::<BTreeSet<_>>();
        removals.extend(self.pending.difference(desired).copied());
        for rule in removals {
            let ipv6 = !rule.cidr.is_ipv4();
            if firewall_rule_exists(net_ns, ipv6, &self.rule_args("-C", rule)).await? {
                firewall(net_ns, ipv6, &self.rule_args("-D", rule)).await?;
            }
            self.installed.remove(&rule);
            self.pending.remove(&rule);
        }
        Ok(())
    }

    async fn release(&mut self, net_ns: &NetNS) -> anyhow::Result<()> {
        for (index, ipv6) in [false, true].into_iter().enumerate() {
            if self.jumped[index] {
                if firewall_rule_exists(net_ns, ipv6, &self.jump_args("-C")).await? {
                    firewall(net_ns, ipv6, &self.jump_args("-D")).await?;
                }
                self.jumped[index] = false;
            }
        }
        self.remove_obsolete(net_ns, &BTreeSet::new()).await?;
        let controls = self
            .control_installed
            .union(&self.control_pending)
            .copied()
            .collect::<Vec<_>>();
        for rule in controls {
            if firewall_rule_exists(net_ns, true, &self.control_rule_args("-C", rule)).await? {
                firewall(net_ns, true, &self.control_rule_args("-D", rule)).await?;
            }
            self.control_installed.remove(&rule);
            self.control_pending.remove(&rule);
        }
        for (index, ipv6) in [false, true].into_iter().enumerate() {
            if self.created[index] {
                // -X succeeds only when empty. If an external actor inserted
                // a rule, preserve it and report cleanup failure, never flush.
                if firewall_rule_exists(net_ns, ipv6, &["-S".to_owned(), self.chain.clone()])
                    .await?
                {
                    firewall(net_ns, ipv6, &["-X".to_owned(), self.chain.clone()]).await?;
                }
                self.created[index] = false;
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
async fn firewall_output(
    net_ns: &NetNS,
    ipv6: bool,
    args: &[String],
) -> anyhow::Result<std::process::Output> {
    let child = net_ns.run(|| {
        tokio::process::Command::new(if ipv6 { "ip6tables" } else { "iptables" })
            .args(["-w", "5", "-t", "filter"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
    })?;
    Ok(child.wait_with_output().await?)
}

#[cfg(target_os = "linux")]
async fn firewall(net_ns: &NetNS, ipv6: bool, args: &[String]) -> anyhow::Result<()> {
    let output = firewall_output(net_ns, ipv6, args).await?;
    anyhow::ensure!(
        output.status.success(),
        "{} {:?}: {}",
        if ipv6 { "ip6tables" } else { "iptables" },
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(target_os = "linux")]
async fn firewall_rule_exists(net_ns: &NetNS, ipv6: bool, args: &[String]) -> anyhow::Result<bool> {
    let output = firewall_output(net_ns, ipv6, args).await?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    anyhow::bail!(
        "{} {:?}: {}",
        if ipv6 { "ip6tables" } else { "iptables" },
        args,
        String::from_utf8_lossy(&output.stderr)
    )
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct ManagedPolicyTable {
    priority: [u32; 2],
    bit: u32,
}

#[cfg(target_os = "linux")]
fn linux_allocation_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(target_os = "linux")]
fn managed_tables() -> &'static Mutex<std::collections::BTreeMap<(u64, u32), ManagedPolicyTable>> {
    static TABLES: std::sync::OnceLock<
        Mutex<std::collections::BTreeMap<(u64, u32), ManagedPolicyTable>>,
    > = std::sync::OnceLock::new();
    TABLES.get_or_init(Mutex::default)
}

#[cfg(target_os = "linux")]
impl Drop for LinuxPolicyTable {
    fn drop(&mut self) {
        // Drop the on-disk claim but never the kernel state: a drop without
        // release() models a crashed owner whose residue the startup sweep
        // reclaims by signature.
        self.remove_lease();
        managed_tables()
            .lock()
            .unwrap()
            .remove(&(self.namespace, self.table));
        managed_tables()
            .lock()
            .unwrap()
            .remove(&(self.namespace, self.blackhole_table));
    }
}

/// EasyTier's dedicated policy table id segment: above the well-known tables
/// and below `main`'s reserved id, so a rule referencing one of these ids
/// with the owner signature below is attributable to this code.
#[cfg(target_os = "linux")]
const LINUX_POLICY_TABLE_RANGE: std::ops::Range<u32> = 20000..32766;

#[cfg(target_os = "linux")]
fn is_policy_table(table: u32) -> bool {
    LINUX_POLICY_TABLE_RANGE.contains(&table)
}

/// On-disk claim over one activation's kernel state, used to distinguish a
/// dead owner's residue from a concurrently running instance's live rules.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct LinuxPolicyLease {
    boot_id: String,
    pid: u32,
    namespace: u64,
    capture_table: u32,
    blackhole_table: u32,
    /// [capture, blackhole] x [IPv4, IPv6] policy rule priorities.
    priorities: [[u32; 2]; 2],
    bit: u32,
}

#[cfg(target_os = "linux")]
fn linux_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[cfg(target_os = "linux")]
fn linux_pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(target_os = "linux")]
fn lease_owner_alive(lease: &LinuxPolicyLease, current_boot_id: &str) -> bool {
    lease.boot_id == current_boot_id && linux_pid_alive(lease.pid)
}

#[cfg(target_os = "linux")]
fn policy_lease_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("EASYTIER_VPN_POLICY_LEASE_DIR") {
        let dir = PathBuf::from(dir);
        return std::fs::create_dir_all(&dir).ok().map(|()| dir);
    }
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let uid = std::fs::metadata("/proc/self")
            .map(|metadata| {
                use std::os::unix::fs::MetadataExt;
                metadata.uid()
            })
            .unwrap_or(0);
        let fallback = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(format!("easytier-vpn-leases-{uid}"));
        [PathBuf::from("/run/easytier"), fallback]
            .into_iter()
            .find(|dir| {
                if std::fs::create_dir_all(dir).is_err() {
                    return false;
                }
                let probe = dir.join(".lease-probe");
                let writable = std::fs::File::create(&probe).is_ok();
                let _ = std::fs::remove_file(&probe);
                writable
            })
    })
    .clone()
}

#[cfg(target_os = "linux")]
fn policy_lease_path(dir: &Path, pid: u32, namespace: u64, table: u32) -> PathBuf {
    dir.join(format!("lease-{pid}-{namespace}-{table}.json"))
}

/// The exact shape `LinuxPolicyTable::activate` installs: loopback-sourced,
/// unmarked value with a single reserved bypass mask bit.
#[cfg(target_os = "linux")]
fn is_easytier_policy_rule(rule: &serde_json::Value) -> bool {
    rule.get("iif").and_then(serde_json::Value::as_str) == Some("lo")
        && rule.get("fwmark").and_then(parse_u32) == Some(0)
        && rule
            .get("fwmask")
            .and_then(parse_u32)
            .is_some_and(|mask| mask.is_power_of_two())
}

/// Extract guard-owner table ids from an `iptables -S` listing: chain
/// declarations plus the OUTPUT jumps that reference them.
#[cfg(target_os = "linux")]
fn parse_easytier_guard_tables(listing: &str) -> BTreeSet<u32> {
    let mut tables = BTreeSet::new();
    for line in listing.lines() {
        let target = if let Some(rest) = line.strip_prefix("-N ") {
            rest.split_whitespace().next().unwrap_or_default()
        } else if let Some(rest) = line.strip_prefix("-A OUTPUT ") {
            match rest.split_once(" -j ") {
                Some((_, target)) => target.trim(),
                None => continue,
            }
        } else {
            continue;
        };
        let Some(number) = target.strip_prefix("ETVPN_") else {
            continue;
        };
        let Ok(table) = number.parse() else {
            continue;
        };
        if is_policy_table(table) {
            tables.insert(table);
        }
    }
    tables
}

/// Remove one activation's OUTPUT guard without an owner instance: the jump,
/// then the whole chain. Unlike [`LinuxOutputGuard::release`] this may flush:
/// the owner is provably gone and the `ETVPN_<table>` name belongs to this
/// code's reserved table segment, so no live owner's rules can live there.
#[cfg(target_os = "linux")]
async fn teardown_stale_output_guard(net_ns: &NetNS, table: u32) {
    let chain = format!("ETVPN_{table}");
    let comment = format!("easytier-vpn-{table}");
    let jump = |operation: &str| {
        [
            operation.to_owned(),
            "OUTPUT".to_owned(),
            "-m".to_owned(),
            "comment".to_owned(),
            "--comment".to_owned(),
            comment.clone(),
            "-j".to_owned(),
            chain.clone(),
        ]
    };
    for ipv6 in [false, true] {
        if firewall_rule_exists(net_ns, ipv6, &jump("-C"))
            .await
            .unwrap_or(false)
        {
            match firewall(net_ns, ipv6, &jump("-D")).await {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(?error, %chain, "failed to delete stale OUTPUT jump");
                }
            }
        }
        let probe = ["-S".to_owned(), chain.clone()];
        if !matches!(firewall_rule_exists(net_ns, ipv6, &probe).await, Ok(true)) {
            continue;
        }
        for operation in ["-F", "-X"] {
            if let Err(error) = firewall(net_ns, ipv6, &[operation.to_owned(), chain.clone()]).await
            {
                tracing::warn!(?error, %chain, "failed to delete stale guard chain");
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn flush_stale_policy_table(net_ns: &NetNS, table: u32) {
    if !is_policy_table(table) {
        return;
    }
    for family in ["-4", "-6"] {
        if let Err(error) = ip(
            net_ns,
            &[family, "route", "flush", "table", &table.to_string()],
        )
        .await
        {
            tracing::warn!(?error, table, "failed to flush stale policy table");
        }
    }
}

/// Best-effort startup recovery for policy-routing state orphaned by a
/// process that died between activation and cleanup. Live owners claim their
/// tables through lease files (or by being registered in this process);
/// everything else matching the EasyTier signature is removed: policy rules,
/// OUTPUT guard chains, and dedicated-table routes. Individual failures warn
/// and never block startup.
#[cfg(target_os = "linux")]
async fn sweep_stale_policy_state(net_ns: &NetNS, namespace: u64, lease_dir: &Path) {
    let _allocation = linux_allocation_gate().lock().await;
    let boot_id = linux_boot_id();
    let mut claimed: BTreeSet<u32> = managed_tables()
        .lock()
        .unwrap()
        .keys()
        .filter(|(owner, _)| *owner == namespace)
        .map(|(_, table)| *table)
        .collect();
    let mut stale_tables: BTreeSet<u32> = BTreeSet::new();
    let mut stale_leases = 0usize;
    let entries: Vec<_> = match std::fs::read_dir(lease_dir) {
        Ok(entries) => entries.flatten().collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(?error, ?lease_dir, "failed to list VPN policy leases");
            Vec::new()
        }
    };
    for entry in entries {
        let path = entry.path();
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                tracing::warn!(?error, ?path, "failed to read VPN policy lease");
                continue;
            }
        };
        let lease = match serde_json::from_str::<LinuxPolicyLease>(&contents) {
            Ok(lease) => lease,
            Err(error) => {
                tracing::warn!(?error, ?path, "discarding malformed VPN policy lease");
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        if lease.namespace != namespace {
            continue; // The claimed state lives in another network namespace.
        }
        if lease_owner_alive(&lease, &boot_id) {
            claimed.insert(lease.capture_table);
            claimed.insert(lease.blackhole_table);
            continue;
        }
        let mark = format!("0/{:#x}", lease.bit);
        for (index, family) in ["-4", "-6"].into_iter().enumerate() {
            for (table, priority) in [
                (lease.capture_table, lease.priorities[0][index]),
                (lease.blackhole_table, lease.priorities[1][index]),
            ] {
                if let Err(error) = ip(
                    net_ns,
                    &[
                        family,
                        "rule",
                        "del",
                        "priority",
                        &priority.to_string(),
                        "iif",
                        "lo",
                        "fwmark",
                        &mark,
                        "lookup",
                        &table.to_string(),
                    ],
                )
                .await
                {
                    tracing::warn!(?error, table, "failed to delete stale policy rule");
                }
            }
        }
        stale_tables.insert(lease.capture_table);
        stale_tables.insert(lease.blackhole_table);
        stale_leases += 1;
        let _ = std::fs::remove_file(&path);
    }
    // Signature residue: state installed by builds without lease files, or by
    // an owner in this process dropped mid-flight (its lease file is removed
    // on drop while the kernel state may outlive the object).
    let mut stale_rules = 0usize;
    for family in ["-4", "-6"] {
        let rules: Vec<serde_json::Value> =
            match ip(net_ns, &[family, "-j", "-N", "rule", "show"]).await {
                Ok(output) => match serde_json::from_slice(&output) {
                    Ok(rules) => rules,
                    Err(error) => {
                        tracing::warn!(?error, "failed to parse policy rules during sweep");
                        continue;
                    }
                },
                Err(error) => {
                    tracing::warn!(?error, "failed to list policy rules during sweep");
                    continue;
                }
            };
        for rule in rules {
            let (Some(table), Some(priority)) = (
                rule.get("table").and_then(parse_u32),
                rule.get("priority").and_then(parse_u32),
            ) else {
                continue;
            };
            if !is_policy_table(table)
                || claimed.contains(&table)
                || !is_easytier_policy_rule(&rule)
            {
                continue;
            }
            let mark = format!(
                "0/{:#x}",
                rule.get("fwmask").and_then(parse_u32).unwrap_or(0)
            );
            match ip(
                net_ns,
                &[
                    family,
                    "rule",
                    "del",
                    "priority",
                    &priority.to_string(),
                    "iif",
                    "lo",
                    "fwmark",
                    &mark,
                    "lookup",
                    &table.to_string(),
                ],
            )
            .await
            {
                Ok(_) => {
                    stale_rules += 1;
                    stale_tables.insert(table);
                }
                Err(error) => {
                    tracing::warn!(?error, table, "failed to sweep stale policy rule");
                }
            }
        }
    }
    let mut stale_chains = 0usize;
    for ipv6 in [false, true] {
        let listing = match firewall_output(net_ns, ipv6, &["-S".to_owned()]).await {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                tracing::warn!(
                    "listing {} chains failed: {}",
                    if ipv6 { "ip6tables" } else { "iptables" },
                    String::from_utf8_lossy(&output.stderr)
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(?error, "listing firewall chains failed during sweep");
                continue;
            }
        };
        for table in parse_easytier_guard_tables(&String::from_utf8_lossy(&listing.stdout)) {
            if !claimed.contains(&table) {
                teardown_stale_output_guard(net_ns, table).await;
                stale_chains += 1;
                stale_tables.insert(table);
            }
        }
    }
    for table in stale_tables {
        flush_stale_policy_table(net_ns, table).await;
    }
    if stale_leases + stale_rules + stale_chains > 0 {
        tracing::info!(
            stale_leases,
            stale_rules,
            stale_chains,
            namespace,
            "swept stale VPN policy routing state"
        );
    }
}

#[cfg(target_os = "linux")]
impl LinuxPolicyTable {
    async fn prepare(net_ns: &NetNS, namespace: u64, configured_mark: u32) -> anyhow::Result<Self> {
        let mut tables = BTreeSet::new();
        let mut masks = Vec::new();
        let mut priority = [1, 1];
        let mut blackhole_priority = [2, 2];
        let managed = managed_tables().lock().unwrap().clone();
        for (ns, table) in managed.keys() {
            if *ns == namespace {
                tables.insert(*table);
            }
        }
        for (index, family) in ["-4", "-6"].into_iter().enumerate() {
            let mut occupied = managed
                .iter()
                .filter(|((ns, _), _)| *ns == namespace)
                .map(|(_, managed)| managed.priority[index])
                .collect::<BTreeSet<_>>();
            let mut local_priority = 0;
            let mut external_priority = u32::MAX;
            let rules: Vec<serde_json::Value> =
                serde_json::from_slice(&ip(net_ns, &[family, "-j", "-N", "rule", "show"]).await?)?;
            for rule in rules {
                let table = rule.get("table").and_then(parse_u32);
                let pref = rule.get("priority").and_then(parse_u32);
                let owned = table
                    .and_then(|table| managed.get(&(namespace, table)))
                    .is_some_and(|managed| {
                        pref == Some(managed.priority[index])
                            && rule.get("fwmark").and_then(parse_u32) == Some(0)
                            && rule.get("fwmask").and_then(parse_u32) == Some(managed.bit)
                            && rule.get("iif").and_then(serde_json::Value::as_str) == Some("lo")
                    });
                if owned { /* Owned rules use the shared namespace bypass bit. */
                } else if let Some(mask) = rule.get("fwmask").and_then(parse_u32) {
                    masks.push(mask);
                } else if rule.get("fwmark").is_some() {
                    masks.push(u32::MAX);
                }
                if let Some(table) = rule.get("table").and_then(parse_u32) {
                    tables.insert(table);
                }
                if let Some(pref) = rule.get("priority").and_then(parse_u32) {
                    occupied.insert(pref);
                    if table == Some(255)
                        || rule.get("table").and_then(serde_json::Value::as_str) == Some("local")
                    {
                        local_priority = local_priority.max(pref);
                    } else if !owned {
                        external_priority = external_priority.min(pref);
                    }
                }
            }
            let capture_priority = (local_priority.saturating_add(1)..external_priority)
                .find(|pref| {
                    pref.checked_add(1).is_some_and(|next| {
                        next < external_priority
                            && !occupied.contains(pref)
                            && !occupied.contains(&next)
                    })
                })
                .ok_or_else(|| anyhow::anyhow!("external policy rules leave no two adjacent safe VPN priorities after local delivery and before external routing"))?;
            priority[index] = capture_priority;
            blackhole_priority[index] = capture_priority + 1;
            occupied.insert(capture_priority);
            occupied.insert(capture_priority + 1);
            let routes: Vec<serde_json::Value> = serde_json::from_slice(
                &ip(
                    net_ns,
                    &[family, "-j", "-N", "route", "show", "table", "all"],
                )
                .await?,
            )?;
            for route in routes {
                if let Some(table) = route.get("table").and_then(parse_u32) {
                    tables.insert(table);
                }
            }
        }
        let mut free_tables = LINUX_POLICY_TABLE_RANGE
            .clone()
            .filter(|table| !tables.contains(table));
        let table = free_tables
            .next()
            .ok_or_else(|| anyhow::anyhow!("no unused dedicated VPN capture routing table"))?;
        let blackhole_table = free_tables
            .next()
            .ok_or_else(|| anyhow::anyhow!("no unused dedicated VPN blackhole routing table"))?;
        let bit = if let Some(bit) = socket_protector::active_bit(namespace) {
            anyhow::ensure!(
                configured_mark & bit == 0 && masks.iter().all(|mask| mask & bit == 0),
                "existing socket bypass bit conflicts with an external fwmark rule or configured mark"
            );
            bit
        } else {
            socket_protector::choose_bypass_bit(
                configured_mark | socket_protector::used_socket_marks(namespace)?,
                masks,
            )?
        };
        {
            let mut managed_tables = managed_tables().lock().unwrap();
            managed_tables.insert((namespace, table), ManagedPolicyTable { priority, bit });
            managed_tables.insert(
                (namespace, blackhole_table),
                ManagedPolicyTable {
                    priority: blackhole_priority,
                    bit,
                },
            );
        }
        Ok(Self {
            namespace,
            table,
            blackhole_table,
            priority,
            blackhole_priority,
            bit,
            active: [false, false],
            blackhole_active: [false, false],
            output_guard: LinuxOutputGuard::new(table, bit),
        })
    }

    async fn activate(&mut self, net_ns: &NetNS) -> anyhow::Result<()> {
        let mut installed = false;
        for (index, family) in ["-4", "-6"].into_iter().enumerate() {
            let mark = format!("0/{:#x}", self.bit);
            if !self.blackhole_active[index] {
                ip(
                    net_ns,
                    &[
                        family,
                        "rule",
                        "add",
                        "priority",
                        &self.blackhole_priority[index].to_string(),
                        "iif",
                        "lo",
                        "fwmark",
                        &mark,
                        "lookup",
                        &self.blackhole_table.to_string(),
                    ],
                )
                .await?;
                self.blackhole_active[index] = true;
                installed = true;
            }
            if !self.active[index] {
                ip(
                    net_ns,
                    &[
                        family,
                        "rule",
                        "add",
                        "priority",
                        &self.priority[index].to_string(),
                        "iif",
                        "lo",
                        "fwmark",
                        &mark,
                        "lookup",
                        &self.table.to_string(),
                    ],
                )
                .await?;
                self.active[index] = true;
                installed = true;
            }
        }
        if installed {
            self.record_lease();
        }
        Ok(())
    }

    async fn release(&mut self, net_ns: &NetNS) -> anyhow::Result<()> {
        for (index, family) in ["-4", "-6"].into_iter().enumerate() {
            let mark = format!("0/{:#x}", self.bit);
            if self.active[index] {
                ip(
                    net_ns,
                    &[
                        family,
                        "rule",
                        "del",
                        "priority",
                        &self.priority[index].to_string(),
                        "iif",
                        "lo",
                        "fwmark",
                        &mark,
                        "lookup",
                        &self.table.to_string(),
                    ],
                )
                .await?;
                self.active[index] = false;
            }
            if self.blackhole_active[index] {
                ip(
                    net_ns,
                    &[
                        family,
                        "rule",
                        "del",
                        "priority",
                        &self.blackhole_priority[index].to_string(),
                        "iif",
                        "lo",
                        "fwmark",
                        &mark,
                        "lookup",
                        &self.blackhole_table.to_string(),
                    ],
                )
                .await?;
                self.blackhole_active[index] = false;
            }
        }
        Ok(())
    }

    fn lease(&self) -> LinuxPolicyLease {
        LinuxPolicyLease {
            boot_id: linux_boot_id(),
            pid: std::process::id(),
            namespace: self.namespace,
            capture_table: self.table,
            blackhole_table: self.blackhole_table,
            priorities: [self.priority, self.blackhole_priority],
            bit: self.bit,
        }
    }

    /// Claim the kernel state cross-process so a later startup can tell this
    /// activation's live rules from a dead owner's residue. Failure is not
    /// fatal: the in-process registry still claims the tables.
    fn record_lease(&self) {
        let Some(dir) = policy_lease_dir() else {
            return;
        };
        let path = policy_lease_path(&dir, std::process::id(), self.namespace, self.table);
        match serde_json::to_string(&self.lease()) {
            Ok(contents) => {
                if let Err(error) = std::fs::write(&path, contents) {
                    tracing::warn!(?error, ?path, "failed to record VPN policy lease");
                }
            }
            Err(error) => tracing::warn!(?error, "failed to encode VPN policy lease"),
        }
    }

    fn remove_lease(&self) {
        let Some(dir) = policy_lease_dir() else {
            return;
        };
        let _ = std::fs::remove_file(policy_lease_path(
            &dir,
            std::process::id(),
            self.namespace,
            self.table,
        ));
    }
}

#[cfg(target_os = "linux")]
fn parse_u32(value: &serde_json::Value) -> Option<u32> {
    if let Some(value) = value.as_u64() {
        return u32::try_from(value).ok();
    }
    let text = value.as_str()?;
    if let Some(hex) = text.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).ok()
    } else {
        text.parse().ok()
    }
}

#[cfg(target_os = "linux")]
fn linux_route_exists(
    routes: &[serde_json::Value],
    destination: &str,
    route_type: &str,
    ifname: Option<&str>,
    metric: Option<u32>,
) -> bool {
    routes.iter().any(|route| {
        route
            .get("dst")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|route_destination| {
                linux_route_destination_matches(route_destination, destination)
            })
            && route
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unicast")
                == route_type
            && match route_type {
                "unreachable" => true,
                "unicast" => ifname.is_some_and(|ifname| {
                    route.get("dev").and_then(serde_json::Value::as_str) == Some(ifname)
                }),
                _ => ifname.is_none_or(|ifname| {
                    route.get("dev").and_then(serde_json::Value::as_str) == Some(ifname)
                }),
            }
            && metric.is_none_or(|metric| route.get("metric").and_then(parse_u32) == Some(metric))
    })
}

#[cfg(target_os = "linux")]
fn linux_route_destination_matches(route_destination: &str, destination: &str) -> bool {
    fn parse_destination(value: &str) -> Option<VpnCidr> {
        value.parse::<VpnCidr>().ok().or_else(|| {
            let address = value.parse::<std::net::IpAddr>().ok()?;
            let prefix = if address.is_ipv4() { 32 } else { 128 };
            format!("{address}/{prefix}").parse().ok()
        })
    }

    matches!(
        (parse_destination(route_destination), parse_destination(destination)),
        (Some(route_destination), Some(destination)) if route_destination == destination
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_output_control_exceptions_only_cover_scoped_ipv6_discovery() {
        let rules = linux_ipv6_control_rules();
        assert_eq!(
            rules
                .iter()
                .map(|rule| rule.icmp_type)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([133, 134, 135, 136]),
        );
        let mut guard = LinuxOutputGuard::new(20000, 0x80);
        guard.ifname = "et-tun".to_owned();
        for rule in rules {
            match rule.icmp_type {
                133 | 134 => {
                    assert_eq!(rule.hop_limit, 255);
                    assert!(matches!(rule.destination, Some("fe80::/10" | "ff02::/16")));
                    if rule.icmp_type == 134 {
                        assert_eq!(rule.source, Some("fe80::/10"));
                    }
                }
                135 | 136 => {
                    assert_eq!(rule.hop_limit, 255);
                    assert_eq!(
                        rule.destination, None,
                        "global unicast neighbors need ND too"
                    );
                }
                _ => unreachable!(),
            }
            let args = guard.control_rule_args("-A", rule);
            assert!(args.windows(2).any(|args| args == ["--mark", "0/0x80"]));
            assert!(args.windows(3).any(|args| args == ["!", "-o", "et-tun"]));
            assert!(args.windows(2).any(|args| args == ["-p", "ipv6-icmp"]));
            assert!(args.windows(2).any(|args| args == ["--icmpv6-type", format!("{}/0", rule.icmp_type).as_str()]));
            assert!(args.ends_with(&["-j".to_owned(), "RETURN".to_owned()]));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_output_partition_only_exempts_known_overlay_local_delivery() {
        use easytier_core::config::vpn::{
            VpnConfig, VpnRuleConfig,
            routes::{VpnRouteExclusions, compile_vpn_routes},
        };
        let config = VpnConfig {
            ignore_private: false,
            rules: ["127.0.0.0/8", "::1/128", "203.0.113.0/25"]
                .into_iter()
                .map(|cidr| VpnRuleConfig {
                    cidr: cidr.parse().unwrap(),
                    action: VpnRuleAction::Block,
                    exits: Vec::new(),
                    priority: 0,
                })
                .collect(),
            ..Default::default()
        };
        let overlay = ["10.66.0.0/24", "fd66::/64", "203.0.113.128/25"]
            .into_iter()
            .map(|cidr| cidr.parse().unwrap())
            .collect::<BTreeSet<_>>();
        let plan = compile_vpn_routes(
            &config,
            &VpnRouteExclusions {
                overlay: overlay.iter().copied().collect(),
                physical: Vec::new(),
            },
            &[],
        );
        assert!(
            plan.capture_routes
                .contains(&"203.0.113.0/24".parse().unwrap())
        );
        assert_eq!(linux_local_overlay(&plan), overlay);
        let rules = linux_output_rules(&plan.capture_routes.into_iter().collect(), &overlay);
        let mut guard = LinuxOutputGuard::new(20000, 0x80);
        guard.ifname = "et-tun".to_owned();
        for address in ["127.0.0.1", "::1", "203.0.113.2"] {
            let address = address.parse().unwrap();
            let matching = rules
                .iter()
                .filter(|rule| rule.cidr.contains(&address))
                .collect::<Vec<_>>();
            assert_eq!(matching.len(), 1);
            assert!(!matching[0].local_overlay);
            assert!(
                !guard
                    .rule_args("-A", *matching[0])
                    .iter()
                    .any(|arg| arg == "LOCAL")
            );
        }
        for address in ["10.66.0.1", "fd66::1", "203.0.113.200"] {
            let address = address.parse().unwrap();
            let matching = rules
                .iter()
                .filter(|rule| rule.cidr.contains(&address))
                .collect::<Vec<_>>();
            assert_eq!(matching.len(), 1);
            assert!(matching[0].local_overlay);
            assert!(
                guard
                    .rule_args("-A", *matching[0])
                    .iter()
                    .any(|arg| arg == "LOCAL")
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_output_partition_clips_overlay_to_captured_regions() {
        let capture = ["10.66.0.0/24", "fd66::/64", "203.0.113.0/24"]
            .into_iter()
            .map(|cidr| cidr.parse().unwrap())
            .collect();
        let overlay = ["10.66.0.0/16", "fd66::/64"]
            .into_iter()
            .map(|cidr| cidr.parse().unwrap())
            .collect();
        assert_eq!(
            linux_output_rules(&capture, &overlay),
            [
                LinuxOutputRule {
                    cidr: "10.66.0.0/24".parse().unwrap(),
                    local_overlay: true
                },
                LinuxOutputRule {
                    cidr: "fd66::/64".parse().unwrap(),
                    local_overlay: true
                },
                LinuxOutputRule {
                    cidr: "203.0.113.0/24".parse().unwrap(),
                    local_overlay: false
                },
            ]
            .into_iter()
            .collect(),
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_output_guard_tracks_overlay_to_block_without_a_cidr_change() {
        let cidr = "203.0.113.0/24".parse().unwrap();
        let overlay_rule = LinuxOutputRule {
            cidr,
            local_overlay: true,
        };
        let block_rule = LinuxOutputRule {
            cidr,
            local_overlay: false,
        };
        let mut guard = LinuxOutputGuard::new(20000, 0x80);
        guard.created = [true; 2];
        guard.jumped = [true; 2];
        guard.control_installed = linux_ipv6_control_rules();
        guard.installed.insert(overlay_rule);
        let desired = BTreeSet::from([block_rule]);
        assert!(!guard.ready_for(&desired));
        assert_eq!(
            desired
                .difference(&guard.installed)
                .copied()
                .collect::<Vec<_>>(),
            [block_rule]
        );
        assert_eq!(
            guard
                .installed
                .difference(&desired)
                .copied()
                .collect::<Vec<_>>(),
            [overlay_rule]
        );
    }

    #[test]
    fn split_defaults_leave_physical_default_routes_owned_by_the_host() {
        assert_eq!(
            split_default("0.0.0.0/0".parse().unwrap())
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["0.0.0.0/1", "128.0.0.0/1"]
        );
        assert_eq!(
            split_default("::/0".parse().unwrap())
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["::/1", "8000::/1"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_route_exists_matches_normalized_destinations_and_kernel_unreachable_devices() {
        let routes = [
            serde_json::json!({
                "dst": "203.0.113.7",
                "type": "unreachable",
                "protocol": "static",
                "flags": []
            }),
            serde_json::json!({
                "dst": "2001:db8:1::7",
                "dev": "lo",
                "type": "unreachable",
                "protocol": "static",
                "metric": 1024,
                "flags": [],
                "pref": "medium"
            }),
            serde_json::json!({
                "dst": "203.0.113.0/25",
                "type": "unreachable",
                "protocol": "static",
                "flags": []
            }),
            serde_json::json!({
                "dst": "2001:db8:1::/64",
                "dev": "lo",
                "type": "unreachable",
                "protocol": "static",
                "metric": 1024,
                "flags": [],
                "pref": "medium"
            }),
            serde_json::json!({
                "dst": "203.0.113.0/25",
                "dev": "eth0",
                "type": "unicast",
                "protocol": "static",
                "flags": []
            }),
        ];

        assert!(linux_route_exists(
            &routes,
            "203.0.113.7/32",
            "unreachable",
            None,
            None,
        ));
        assert!(linux_route_exists(
            &routes,
            "2001:db8:1::7/128",
            "unreachable",
            None,
            None,
        ));
        assert!(linux_route_exists(
            &routes,
            "203.0.113.0/25",
            "unreachable",
            None,
            None,
        ));
        assert!(linux_route_exists(
            &routes,
            "2001:db8:1::/64",
            "unreachable",
            None,
            None,
        ));
        assert!(!linux_route_exists(
            &routes,
            "203.0.113.0/25",
            "unicast",
            Some("et-tun"),
            None,
        ));
        assert!(linux_route_exists(
            &routes,
            "203.0.113.0/25",
            "unicast",
            Some("eth0"),
            None,
        ));
        assert!(!linux_route_exists(
            &routes,
            "203.0.113.0/25",
            "unicast",
            None,
            None,
        ));
    }

    #[cfg(target_os = "linux")]
    async fn assert_output_guard_state(
        net_ns: &NetNS,
        table: u32,
        expected: bool,
    ) -> anyhow::Result<()> {
        let chain = format!("ETVPN_{table}");
        let comment = format!("easytier-vpn-{table}");
        for ipv6 in [false, true] {
            let output_rules =
                firewall_output(net_ns, ipv6, &["-S".to_owned(), "OUTPUT".to_owned()]).await?;
            anyhow::ensure!(output_rules.status.success(), "listing OUTPUT rules failed");
            let jump = format!("--comment {comment} -j {chain}");
            assert_eq!(
                String::from_utf8_lossy(&output_rules.stdout)
                    .lines()
                    .any(|rule| rule.contains(&jump)),
                expected,
                "unexpected {} OUTPUT jump state for {chain}",
                if ipv6 { "IPv6" } else { "IPv4" }
            );
            let listing = firewall_output(net_ns, ipv6, &["-S".to_owned()]).await?;
            anyhow::ensure!(
                listing.status.success(),
                "listing {} filter chains failed: {}",
                if ipv6 { "IPv6" } else { "IPv4" },
                String::from_utf8_lossy(&listing.stderr)
            );
            let chain_definition = format!("-N {chain}");
            let chain_rule_prefix = format!("-A {chain} ");
            let chain_lines = String::from_utf8_lossy(&listing.stdout);
            let chain_exists = chain_lines.lines().any(|line| line == chain_definition);
            let chain_has_rules = chain_lines
                .lines()
                .any(|line| line.starts_with(&chain_rule_prefix));
            assert_eq!(
                chain_exists,
                expected,
                "unexpected {} guard chain declaration state for {chain}",
                if ipv6 { "IPv6" } else { "IPv4" }
            );
            assert_eq!(
                chain_has_rules,
                expected,
                "unexpected {} guard chain rows for {chain}",
                if ipv6 { "IPv6" } else { "IPv4" }
            );
            if expected {
                assert!(
                    chain_lines.lines().any(|line| {
                        line.starts_with(&chain_rule_prefix) && line.ends_with(" -j DROP")
                    }),
                    "{} guard chain {chain} has no capture drop rows",
                    if ipv6 { "IPv6" } else { "IPv4" }
                );
            }
        }
        Ok(())
    }

    /// Gated because this creates an isolated network namespace and requires
    /// CAP_SYS_ADMIN/CAP_NET_ADMIN. It never changes the host routing table.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dedicated_table_capture_and_socket_bypass_survive_exit_outage_and_cleanup() {
        if std::env::var_os("EASYTIER_RUN_VPN_ROUTE_KERNEL_TESTS").is_none() {
            return;
        }
        use easytier_core::{
            config::vpn::{VpnConfig, VpnRuleConfig},
            peers::vpn_policy::{VpnDecision, VpnPolicyEngine},
        };
        struct NamespaceCleanup(Vec<String>);
        impl Drop for NamespaceCleanup {
            fn drop(&mut self) {
                for namespace in &self.0 {
                    let _ = std::process::Command::new("ip")
                        .args(["netns", "del", namespace])
                        .output();
                }
            }
        }
        let name = format!(
            "et-vpn-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let peer_name = format!("{name}-peer");
        let client_name = format!("{name}-client");
        let output = tokio::process::Command::new("ip")
            .args(["netns", "add", &name])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "create isolated netns: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut cleanup = NamespaceCleanup(vec![name.clone()]);
        let peer_output = tokio::process::Command::new("ip")
            .args(["netns", "add", &peer_name])
            .output()
            .await
            .unwrap();
        assert!(
            peer_output.status.success(),
            "create physical peer netns: {}",
            String::from_utf8_lossy(&peer_output.stderr)
        );
        cleanup.0.push(peer_name.clone());
        let client_output = tokio::process::Command::new("ip")
            .args(["netns", "add", &client_name])
            .output()
            .await
            .unwrap();
        assert!(
            client_output.status.success(),
            "create TUN client netns: {}",
            String::from_utf8_lossy(&client_output.stderr)
        );
        cleanup.0.push(client_name.clone());
        let net_ns = NetNS::new(Some(name));
        let peer_ns = NetNS::new(Some(peer_name.clone()));
        let client_ns = NetNS::new(Some(client_name.clone()));
        for args in [
            vec!["link", "set", "lo", "up"],
            vec![
                "link",
                "add",
                "physical",
                "type",
                "veth",
                "peer",
                "name",
                "physical-peer",
            ],
            vec!["link", "set", "physical-peer", "netns", peer_name.as_str()],
            vec!["link", "set", "physical", "up"],
            vec!["addr", "add", "192.0.2.2/24", "dev", "physical"],
            vec![
                "addr",
                "add",
                "2001:db8:1::2/64",
                "dev",
                "physical",
                "nodad",
            ],
            vec![
                "route",
                "add",
                "default",
                "via",
                "192.0.2.1",
                "dev",
                "physical",
            ],
            vec![
                "link",
                "add",
                "et-tun",
                "type",
                "veth",
                "peer",
                "name",
                "client-tun",
            ],
            vec!["link", "set", "client-tun", "netns", client_name.as_str()],
            vec!["link", "set", "et-tun", "up"],
            vec!["addr", "add", "10.66.0.1/24", "dev", "et-tun"],
            vec![
                "rule",
                "add",
                "priority",
                "100",
                "fwmark",
                "0x42/0xff",
                "lookup",
                "50",
            ],
            vec![
                "route",
                "add",
                "default",
                "via",
                "192.0.2.1",
                "dev",
                "physical",
                "table",
                "200",
            ],
            vec![
                "route",
                "add",
                "10.66.0.0/24",
                "dev",
                "et-tun",
                "table",
                "200",
            ],
            vec![
                "-6",
                "route",
                "add",
                "2001:db8:1::/64",
                "dev",
                "physical",
                "table",
                "200",
            ],
            vec!["rule", "add", "priority", "101", "lookup", "200"],
            vec!["-6", "rule", "add", "priority", "101", "lookup", "200"],
        ] {
            ip(&net_ns, &args).await.unwrap();
        }
        for args in [
            vec!["link", "set", "lo", "up"],
            vec!["link", "set", "client-tun", "up"],
            vec!["addr", "add", "10.66.0.2/24", "dev", "client-tun"],
            vec![
                "route",
                "add",
                "203.0.113.0/24",
                "via",
                "10.66.0.1",
                "dev",
                "client-tun",
            ],
        ] {
            ip(&client_ns, &args).await.unwrap();
        }
        net_ns
            .run(|| -> std::io::Result<()> {
                std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")?;
                for interface in ["all", "default", "physical", "et-tun"] {
                    std::fs::write(
                        format!("/proc/sys/net/ipv4/conf/{interface}/rp_filter"),
                        "0",
                    )?;
                }
                Ok(())
            })
            .unwrap();
        for args in [
            vec!["link", "set", "lo", "up"],
            vec!["link", "set", "physical-peer", "up"],
            vec!["addr", "add", "192.0.2.1/24", "dev", "physical-peer"],
            vec![
                "addr",
                "add",
                "2001:db8:1::1/64",
                "dev",
                "physical-peer",
                "nodad",
            ],
            vec!["addr", "add", "203.0.113.1/32", "dev", "lo"],
            vec!["addr", "add", "8.8.8.8/32", "dev", "lo"],
            vec![
                "route",
                "add",
                "10.66.0.0/24",
                "via",
                "192.0.2.2",
                "dev",
                "physical-peer",
            ],
        ] {
            ip(&peer_ns, &args).await.unwrap();
        }
        let udp_server = peer_ns
            .run(|| {
                std::net::UdpSocket::bind("203.0.113.1:0".parse::<std::net::SocketAddr>().unwrap())
            })
            .unwrap();
        udp_server
            .set_read_timeout(Some(std::time::Duration::from_millis(250)))
            .unwrap();
        let udp_addr = udp_server.local_addr().unwrap();
        let ipv6_udp_server = peer_ns
            .run(|| {
                std::net::UdpSocket::bind(
                    "[2001:db8:1::1]:0".parse::<std::net::SocketAddr>().unwrap(),
                )
            })
            .unwrap();
        ipv6_udp_server
            .set_read_timeout(Some(std::time::Duration::from_millis(250)))
            .unwrap();
        let ipv6_udp_addr = ipv6_udp_server.local_addr().unwrap();
        let dns_server = peer_ns
            .run(|| {
                std::net::UdpSocket::bind("8.8.8.8:53".parse::<std::net::SocketAddr>().unwrap())
            })
            .unwrap();
        let dns_addr = dns_server.local_addr().unwrap();
        dns_server
            .set_read_timeout(Some(std::time::Duration::from_millis(250)))
            .unwrap();
        let tcp_server = peer_ns
            .run(|| {
                std::net::TcpListener::bind(
                    "203.0.113.1:0".parse::<std::net::SocketAddr>().unwrap(),
                )
            })
            .unwrap();
        tcp_server.set_nonblocking(true).unwrap();
        let tcp_server = tokio::net::TcpListener::from_std(tcp_server).unwrap();
        let tcp_addr = tcp_server.local_addr().unwrap();
        let existing = net_ns
            .run(|| {
                socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                )
            })
            .unwrap();
        existing.set_mark(0x42).unwrap();
        existing
            .bind(&"0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap().into())
            .unwrap();
        let _existing_owner = crate::socket_protector::protect_native_socket(&existing, true)
            .await
            .unwrap();
        let config = VpnConfig {
            ignore_private: false,
            dns_protect: true,
            ipv6_protect: true,
            rules: vec![VpnRuleConfig {
                cidr: "203.0.113.0/24".parse().unwrap(),
                action: VpnRuleAction::Exit,
                exits: vec!["10.66.0.2".parse().unwrap()],
                priority: 0,
            }],
            ..Default::default()
        };
        let engine = VpnPolicyEngine::new(config).unwrap();
        engine.update_system_dns_servers(vec!["8.8.8.8".parse().unwrap()]);
        let desired = engine
            .route_plan()
            .0
            .capture_routes
            .into_iter()
            .flat_map(split_default)
            .collect::<BTreeSet<_>>();
        let ifcfg: BoxedIfCfg = Arc::new(crate::common::ifcfg::IfConfiger {});
        let mut owner = RouteOwner::default();
        owner
            .reconcile(&ifcfg, &net_ns, "et-tun", &desired, 0x42)
            .await
            .unwrap();
        let table = owner.linux.as_ref().unwrap().table;
        let blackhole_table = owner.linux.as_ref().unwrap().blackhole_table;
        let bit = owner.linux.as_ref().unwrap().bit;
        let capture_priority = owner.linux.as_ref().unwrap().priority[0];
        let blackhole_priority = owner.linux.as_ref().unwrap().blackhole_priority[0];
        let rules: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(&net_ns, &["-4", "-j", "-N", "rule", "show"])
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(rules.iter().any(|rule| {
            rule.get("priority").and_then(parse_u32) == Some(capture_priority)
                && rule.get("table").and_then(parse_u32) == Some(table)
                && rule.get("iif").and_then(serde_json::Value::as_str) == Some("lo")
                && rule.get("fwmark").and_then(parse_u32) == Some(0)
                && rule.get("fwmask").and_then(parse_u32) == Some(bit)
        }));
        assert!(rules.iter().any(|rule| {
            rule.get("priority").and_then(parse_u32) == Some(blackhole_priority)
                && rule.get("table").and_then(parse_u32) == Some(blackhole_table)
                && rule.get("iif").and_then(serde_json::Value::as_str) == Some("lo")
                && rule.get("fwmark").and_then(parse_u32) == Some(0)
                && rule.get("fwmask").and_then(parse_u32) == Some(bit)
        }));
        assert!(blackhole_priority > capture_priority);
        for cidr in &desired {
            let destination = cidr.to_string();
            let family = family(*cidr);
            let captured_routes: Vec<serde_json::Value> = serde_json::from_slice(
                &ip(
                    &net_ns,
                    &[family, "-j", "route", "show", "table", &table.to_string()],
                )
                .await
                .unwrap(),
            )
            .unwrap();
            assert!(linux_route_exists(
                &captured_routes,
                &destination,
                "unicast",
                Some("et-tun"),
                None,
            ));
            let blackhole_routes: Vec<serde_json::Value> = serde_json::from_slice(
                &ip(
                    &net_ns,
                    &[
                        family,
                        "-j",
                        "route",
                        "show",
                        "table",
                        &blackhole_table.to_string(),
                    ],
                )
                .await
                .unwrap(),
            )
            .unwrap();
            assert!(linux_route_exists(
                &blackhole_routes,
                &destination,
                "unreachable",
                None,
                None,
            ));
        }
        let capture_priority_v6 = owner.linux.as_ref().unwrap().priority[1];
        let rules_v6: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(&net_ns, &["-6", "-j", "-N", "rule", "show"])
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(rules_v6.iter().any(|rule| {
            rule.get("priority").and_then(parse_u32) == Some(capture_priority_v6)
                && rule.get("table").and_then(parse_u32) == Some(table)
                && rule.get("iif").and_then(serde_json::Value::as_str) == Some("lo")
                && rule.get("fwmark").and_then(parse_u32) == Some(0)
                && rule.get("fwmask").and_then(parse_u32) == Some(bit)
        }));
        let blackhole_priority_v6 = owner.linux.as_ref().unwrap().blackhole_priority[1];
        assert!(rules_v6.iter().any(|rule| {
            rule.get("priority").and_then(parse_u32) == Some(blackhole_priority_v6)
                && rule.get("table").and_then(parse_u32) == Some(blackhole_table)
                && rule.get("iif").and_then(serde_json::Value::as_str) == Some("lo")
                && rule.get("fwmark").and_then(parse_u32) == Some(0)
                && rule.get("fwmask").and_then(parse_u32) == Some(bit)
        }));
        assert_eq!(
            existing.mark().unwrap(),
            0x42 | bit,
            "hot takeover must protect existing sockets without erasing original mark"
        );
        let route: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(&net_ns, &["-4", "-j", "route", "get", "203.0.113.1"])
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(route[0]["dev"], "et-tun");
        let forwarded: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(
                &net_ns,
                &[
                    "-4",
                    "-j",
                    "route",
                    "get",
                    "203.0.113.1",
                    "from",
                    "10.66.0.2",
                    "iif",
                    "et-tun",
                ],
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            forwarded[0]["dev"], "physical",
            "exported TUN packets must skip local client capture"
        );
        engine.update_exit_states([("10.66.0.2".parse().unwrap(), Some(42), true, None)]);
        assert_eq!(
            engine.resolve(&"203.0.113.1".parse().unwrap(), 0),
            VpnDecision::Exit(42)
        );
        engine.update_exit_states([("10.66.0.2".parse().unwrap(), Some(42), false, None)]);
        assert_eq!(
            engine.resolve(&"203.0.113.1".parse().unwrap(), 0),
            VpnDecision::Block
        );
        assert_eq!(
            engine.resolve(&"8.8.8.8".parse().unwrap(), 0),
            VpnDecision::Block
        );
        assert_eq!(
            engine.resolve(&"2001:db8:1::1".parse().unwrap(), 0),
            VpnDecision::Block
        );
        owner
            .reconcile(&ifcfg, &net_ns, "et-tun", &desired, 0x42)
            .await
            .unwrap();
        let outage_route: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(&net_ns, &["-4", "-j", "route", "get", "203.0.113.1"])
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            outage_route[0]["dev"], "et-tun",
            "all exits down must keep local capture from falling through to physical"
        );
        let dns_route: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(&net_ns, &["-4", "-j", "route", "get", "8.8.8.8"])
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(dns_route[0]["dev"], "et-tun");
        let ipv6_route: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(
                &net_ns,
                &["-6", "-j", "route", "get", "2001:db8:1::1", "iif", "lo"],
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(ipv6_route[0]["dev"], "et-tun");
        assert_output_guard_state(&net_ns, table, true)
            .await
            .unwrap();
        let mut received = [0u8; 64];
        let forwarded_source = client_ns
            .run(|| {
                std::net::UdpSocket::bind("10.66.0.2:0".parse::<std::net::SocketAddr>().unwrap())
            })
            .unwrap();
        forwarded_source
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        udp_server
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        forwarded_source
            .send_to(b"forwarded-through-physical", udp_addr)
            .unwrap();
        let (forwarded_length, forwarded_peer) = udp_server.recv_from(&mut received).unwrap();
        assert_eq!(
            &received[..forwarded_length],
            b"forwarded-through-physical",
            "TUN-ingress traffic must traverse the physical peer without OUTPUT capture"
        );
        udp_server
            .send_to(b"forwarded-return-path", forwarded_peer)
            .unwrap();
        let (returned_length, _) = forwarded_source.recv_from(&mut received).unwrap();
        assert_eq!(&received[..returned_length], b"forwarded-return-path");
        let unmarked = net_ns
            .run(|| std::net::UdpSocket::bind("0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap()))
            .unwrap();
        unmarked.send_to(b"blocked-capture", udp_addr).unwrap();
        let no_capture = udp_server.recv_from(&mut received);
        assert!(
            matches!(
                no_capture,
                Err(ref error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
            ),
            "unmarked capture datagram reached physical peer during exit outage: {no_capture:?}"
        );
        let dns_query = [
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, b'e',
            b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00, 0x00, 0x01, 0x00,
            0x01,
        ];
        let unmarked_dns = net_ns
            .run(|| std::net::UdpSocket::bind("0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap()))
            .unwrap();
        unmarked_dns.send_to(&dns_query, dns_addr).unwrap();
        let no_dns_capture = dns_server.recv_from(&mut received);
        assert!(
            matches!(
                no_dns_capture,
                Err(ref error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
            ),
            "unmarked public DNS query reached the physical peer: {no_dns_capture:?}"
        );
        let bound_dns = net_ns
            .run(|| {
                socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                )
            })
            .unwrap();
        bound_dns.bind_device(Some(b"physical")).unwrap();
        let _bound_dns_send = bound_dns.send_to(&dns_query, &dns_addr.into());
        let bound_dns_capture = dns_server.recv_from(&mut received);
        assert!(
            matches!(
                bound_dns_capture,
                Err(ref error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
            ),
            "unmarked SO_BINDTODEVICE DNS query reached the physical peer: {bound_dns_capture:?}"
        );
        dns_server
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let dns_client = net_ns
            .run(|| {
                socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                )
            })
            .unwrap();
        dns_client.bind_device(Some(b"physical")).unwrap();
        dns_client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        dns_client.set_mark(0x42).unwrap();
        let _dns_owner = crate::socket_protector::protect_native_socket(&dns_client, true)
            .await
            .unwrap();
        dns_client.connect(&dns_addr.into()).unwrap();
        dns_client.send(&dns_query).unwrap();
        let (dns_length, dns_peer) = dns_server.recv_from(&mut received).unwrap();
        assert_eq!(&received[..dns_length], &dns_query);
        let mut dns_response = dns_query.to_vec();
        dns_response[2] = 0x81;
        dns_response[3] = 0x80;
        dns_response[7] = 1;
        dns_response.extend_from_slice(&[
            0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x04, 1, 2, 3, 4,
        ]);
        dns_server.send_to(&dns_response, dns_peer).unwrap();
        let mut dns_received = [std::mem::MaybeUninit::<u8>::uninit(); 64];
        let dns_response_length = dns_client.recv(&mut dns_received).unwrap();
        let dns_received = unsafe {
            std::slice::from_raw_parts(dns_received.as_ptr().cast::<u8>(), dns_response_length)
        };
        assert_eq!(dns_received, &dns_response);

        let unmarked_ipv6 = net_ns
            .run(|| std::net::UdpSocket::bind("[::]:0".parse::<std::net::SocketAddr>().unwrap()))
            .unwrap();
        unmarked_ipv6
            .send_to(b"blocked-ipv6", ipv6_udp_addr)
            .unwrap();
        let mut ipv6_received = [0u8; 64];
        let no_ipv6_capture = ipv6_udp_server.recv_from(&mut ipv6_received);
        assert!(
            matches!(
                no_ipv6_capture,
                Err(ref error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
            ),
            "unmarked IPv6 datagram reached the physical peer: {no_ipv6_capture:?}"
        );
        let bound_ipv6 = net_ns
            .run(|| {
                socket2::Socket::new(
                    socket2::Domain::IPV6,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                )
            })
            .unwrap();
        bound_ipv6.bind_device(Some(b"physical")).unwrap();
        let _bound_ipv6_send = bound_ipv6.send_to(b"blocked-bound-ipv6", &ipv6_udp_addr.into());
        let no_bound_ipv6_capture = ipv6_udp_server.recv_from(&mut ipv6_received);
        assert!(
            matches!(
                no_bound_ipv6_capture,
                Err(ref error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
            ),
            "unmarked physical-bound IPv6 datagram reached the peer: {no_bound_ipv6_capture:?}"
        );
        ipv6_udp_server
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        ip(&net_ns, &["-6", "neigh", "flush", "dev", "physical"])
            .await
            .unwrap();
        ip(&peer_ns, &["-6", "neigh", "flush", "dev", "physical-peer"])
            .await
            .unwrap();
        let protected_ipv6 = net_ns
            .run(|| {
                socket2::Socket::new(
                    socket2::Domain::IPV6,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                )
            })
            .unwrap();
        protected_ipv6.bind_device(Some(b"physical")).unwrap();
        protected_ipv6
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        protected_ipv6.set_mark(0x42).unwrap();
        let _ipv6_owner = crate::socket_protector::protect_native_socket(&protected_ipv6, true)
            .await
            .unwrap();
        let protected_ipv6_route: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(
                &net_ns,
                &[
                    "-6",
                    "-j",
                    "route",
                    "get",
                    "2001:db8:1::1",
                    "mark",
                    &format!("{:#x}", protected_ipv6.mark().unwrap()),
                ],
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(protected_ipv6_route[0]["dev"], "physical");
        protected_ipv6.connect(&ipv6_udp_addr.into()).unwrap();
        protected_ipv6.send(b"protected-ipv6").unwrap();
        let (ipv6_length, ipv6_peer) = ipv6_udp_server.recv_from(&mut ipv6_received).unwrap();
        assert_eq!(&ipv6_received[..ipv6_length], b"protected-ipv6");
        ipv6_udp_server
            .send_to(b"protected-ipv6-response", ipv6_peer)
            .unwrap();
        let mut ipv6_response = [std::mem::MaybeUninit::<u8>::uninit(); 64];
        let ipv6_response_length = protected_ipv6.recv(&mut ipv6_response).unwrap();
        let ipv6_response = unsafe {
            std::slice::from_raw_parts(ipv6_response.as_ptr().cast::<u8>(), ipv6_response_length)
        };
        assert_eq!(ipv6_response, b"protected-ipv6-response");
        udp_server
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        existing.connect(&udp_addr.into()).unwrap();
        existing.send(b"protected-udp").unwrap();
        let (length, _) = udp_server.recv_from(&mut received).unwrap();
        assert_eq!(&received[..length], b"protected-udp");
        assert_eq!(
            existing.local_addr().unwrap().as_socket().unwrap().ip(),
            "192.0.2.2".parse::<std::net::IpAddr>().unwrap()
        );
        let fresh = net_ns
            .run(|| {
                socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::STREAM,
                    Some(socket2::Protocol::TCP),
                )
            })
            .unwrap();
        fresh.set_mark(0x42).unwrap();
        let _fresh_owner = crate::socket_protector::protect_native_socket(&fresh, true)
            .await
            .unwrap();
        assert_eq!(
            fresh.mark().unwrap(),
            0x42 | bit,
            "new sockets must inherit bypass before first IO"
        );
        let protected_route: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(
                &net_ns,
                &[
                    "-4",
                    "-j",
                    "route",
                    "get",
                    "203.0.113.1",
                    "mark",
                    &format!("{:#x}", fresh.mark().unwrap()),
                ],
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(protected_route[0]["dev"], "physical");
        fresh.set_nonblocking(true).unwrap();
        let connect = fresh.connect(&tcp_addr.into());
        assert!(
            connect.is_ok()
                || connect
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.raw_os_error() == Some(libc::EINPROGRESS))
        );
        assert_eq!(
            fresh.local_addr().unwrap().as_socket().unwrap().ip(),
            "192.0.2.2".parse::<std::net::IpAddr>().unwrap(),
            "first protected TCP SYN must select physical source"
        );
        let (accepted, peer_addr) =
            tokio::time::timeout(std::time::Duration::from_secs(2), tcp_server.accept())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            peer_addr.ip(),
            "192.0.2.2".parse::<std::net::IpAddr>().unwrap()
        );
        drop(accepted);
        let mut second = RouteOwner::default();
        second
            .reconcile(&ifcfg, &net_ns, "et-tun", &desired, 0x42)
            .await
            .unwrap();
        let second_table = second.linux.as_ref().unwrap().table;
        let second_blackhole_table = second.linux.as_ref().unwrap().blackhole_table;
        assert_ne!(table, second.linux.as_ref().unwrap().table);
        assert_eq!(bit, second.linux.as_ref().unwrap().bit);
        assert_output_guard_state(&net_ns, table, true)
            .await
            .unwrap();
        assert_output_guard_state(&net_ns, second_table, true)
            .await
            .unwrap();
        owner
            .reconcile(&ifcfg, &net_ns, "et-tun", &BTreeSet::new(), 0x42)
            .await
            .unwrap();
        assert_output_guard_state(&net_ns, table, false)
            .await
            .unwrap();
        assert_output_guard_state(&net_ns, second_table, true)
            .await
            .unwrap();
        assert_eq!(
            fresh.mark().unwrap(),
            0x42 | bit,
            "another table owner still requires the namespace bypass"
        );
        second
            .reconcile(&ifcfg, &net_ns, "et-tun", &BTreeSet::new(), 0x42)
            .await
            .unwrap();
        assert_output_guard_state(&net_ns, second_table, false)
            .await
            .unwrap();
        assert_eq!(existing.mark().unwrap(), 0x42);
        assert_eq!(fresh.mark().unwrap(), 0x42);
        let rules: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(&net_ns, &["-4", "-j", "-N", "rule", "show"])
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            rules
                .iter()
                .any(|rule| rule.get("priority").and_then(parse_u32) == Some(100)
                    && rule.get("fwmask").and_then(parse_u32) == Some(0xff))
        );
        assert!(!rules.iter().any(|rule| {
            rule.get("table")
                .and_then(parse_u32)
                .is_some_and(|id| (20000..32766).contains(&id))
        }));
        for table_id in [table, blackhole_table, second_table, second_blackhole_table] {
            for family in ["-4", "-6"] {
                let routes: Vec<serde_json::Value> = serde_json::from_slice(
                    &ip(
                        &net_ns,
                        &[
                            family,
                            "-j",
                            "route",
                            "show",
                            "table",
                            &table_id.to_string(),
                        ],
                    )
                    .await
                    .unwrap(),
                )
                .unwrap();
                assert!(
                    routes.is_empty(),
                    "capture table {table_id} retained {family} routes after cleanup: {routes:?}"
                );
            }
        }
        let route: Vec<serde_json::Value> = serde_json::from_slice(
            &ip(&net_ns, &["-4", "-j", "route", "get", "203.0.113.1"])
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(route[0]["dev"], "physical");
        let restored_socket = net_ns
            .run(|| std::net::UdpSocket::bind("0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap()))
            .unwrap();
        restored_socket
            .send_to(b"restored-after-cleanup", udp_addr)
            .unwrap();
        let (restored_length, _) = udp_server.recv_from(&mut received).unwrap();
        assert_eq!(&received[..restored_length], b"restored-after-cleanup");
        ip(&net_ns, &["rule", "add", "priority", "0", "lookup", "200"])
            .await
            .unwrap();
        let mut priority_conflict = RouteOwner::default();
        let error = priority_conflict
            .reconcile(&ifcfg, &net_ns, "et-tun", &desired, 0x42)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("safe VPN priorities"),
            "unexpected priority conflict error: {error:#}"
        );
        assert!(priority_conflict.installed.is_empty());
        ip(&net_ns, &["rule", "del", "priority", "0", "lookup", "200"])
            .await
            .unwrap();
        ip(
            &net_ns,
            &[
                "rule",
                "add",
                "priority",
                "102",
                "fwmark",
                "0x42/0xffffffff",
                "lookup",
                "50",
            ],
        )
        .await
        .unwrap();
        let mut conflict = RouteOwner::default();
        assert!(
            conflict
                .reconcile(&ifcfg, &net_ns, "et-tun", &desired, 0x42)
                .await
                .is_err()
        );
        assert!(conflict.installed.is_empty() && conflict.protection.is_none());
        assert_eq!(
            existing.mark().unwrap(),
            0x42,
            "conflicting activation must preserve original socket mark"
        );
    }

    #[cfg(target_os = "linux")]
    fn test_lease(boot_id: &str, pid: u32) -> LinuxPolicyLease {
        LinuxPolicyLease {
            boot_id: boot_id.to_owned(),
            pid,
            namespace: 7,
            capture_table: 20000,
            blackhole_table: 20001,
            priorities: [[1, 1], [2, 2]],
            bit: 0x80,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn policy_lease_roundtrip() {
        let lease = test_lease("11111111-2222-3333-4444-555555555555", 42);
        let encoded = serde_json::to_string(&lease).unwrap();
        assert_eq!(
            serde_json::from_str::<LinuxPolicyLease>(&encoded).unwrap(),
            lease
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lease_liveness_requires_current_boot_and_live_pid() {
        let boot_id = linux_boot_id();
        assert!(lease_owner_alive(
            &test_lease(&boot_id, std::process::id()),
            &boot_id
        ));
        assert!(!lease_owner_alive(
            &test_lease("00000000-0000-0000-0000-000000000000", std::process::id()),
            &boot_id
        ));
        // One past the kernel's default pid_max: /proc never lists it.
        assert!(!lease_owner_alive(&test_lease(&boot_id, 4194304), &boot_id));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn policy_rule_signature_rejects_foreign_shapes() {
        assert!(is_easytier_policy_rule(&serde_json::json!({
            "iif": "lo",
            "fwmark": 0,
            "fwmask": "0x80",
            "priority": 3,
            "table": 20001,
        })));
        assert!(is_easytier_policy_rule(&serde_json::json!({
            "iif": "lo",
            "fwmark": "0x0",
            "fwmask": 128,
            "priority": 3,
            "table": 20001,
        })));
        for foreign in [
            serde_json::json!({"iif": "lo", "fwmark": 0, "fwmask": "0xff"}),
            serde_json::json!({"iif": "eth0", "fwmark": 0, "fwmask": "0x80"}),
            serde_json::json!({"iif": "lo", "fwmark": 1, "fwmask": "0x80"}),
            serde_json::json!({"iif": "lo", "fwmark": 0, "fwmask": 0}),
        ] {
            assert!(!is_easytier_policy_rule(&foreign), "{foreign}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guard_table_parsing_finds_chains_and_output_jumps() {
        let listing = "\
-N INPUT
-A INPUT -j ACCEPT
-N ETVPN_20000
-A OUTPUT -m comment --comment easytier-vpn-20000 -j ETVPN_20000
-N ETVPN_300
-A FORWARD -j ETVPN_21000
-A OUTPUT -j ETVPN_32766
";
        assert_eq!(
            parse_easytier_guard_tables(listing),
            BTreeSet::from([20000])
        );
    }

    #[cfg(target_os = "linux")]
    async fn policy_rules_reference_any_table(net_ns: &NetNS, tables: &[u32]) -> bool {
        for family in ["-4", "-6"] {
            let rules: Vec<serde_json::Value> = serde_json::from_slice(
                &ip(net_ns, &[family, "-j", "-N", "rule", "show"])
                    .await
                    .unwrap(),
            )
            .unwrap();
            if rules.iter().any(|rule| {
                rule.get("table")
                    .and_then(parse_u32)
                    .is_some_and(|table| tables.contains(&table))
            }) {
                return true;
            }
        }
        false
    }

    /// Gated like the kernel test above: creates a throwaway netns and needs
    /// CAP_SYS_ADMIN/CAP_NET_ADMIN for real ip/iptables state. Proves crash
    /// residue no longer blocks the next activation and that live owners are
    /// never swept.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stale_policy_state_is_swept_and_no_longer_blocks_activation() {
        if std::env::var_os("EASYTIER_RUN_VPN_ROUTE_KERNEL_TESTS").is_none() {
            return;
        }
        struct Cleanup(Vec<String>, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for namespace in &self.0 {
                    let _ = std::process::Command::new("ip")
                        .args(["netns", "del", namespace])
                        .output();
                }
                let _ = std::fs::remove_dir_all(&self.1);
            }
        }
        let name = format!(
            "et-vpn-sweep-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let lease_dir = std::env::temp_dir().join(format!("{name}-leases"));
        std::fs::create_dir_all(&lease_dir).unwrap();
        unsafe {
            std::env::set_var("EASYTIER_VPN_POLICY_LEASE_DIR", &lease_dir);
        }
        let output = tokio::process::Command::new("ip")
            .args(["netns", "add", &name])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "create isolated netns: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _cleanup = Cleanup(vec![name.clone()], lease_dir.clone());
        let net_ns = NetNS::new(Some(name));
        for args in [
            vec!["link", "set", "lo", "up"],
            vec!["link", "add", "et-tun", "type", "dummy"],
            vec!["link", "set", "et-tun", "up"],
        ] {
            ip(&net_ns, &args).await.unwrap();
        }
        let namespace = socket_protector::namespace_for(&net_ns).unwrap();
        let ifcfg: BoxedIfCfg = Arc::new(crate::common::ifcfg::IfConfiger {});
        let desired = BTreeSet::from([
            "203.0.113.0/24".parse().unwrap(),
            "2001:db8:1::/64".parse().unwrap(),
        ]);
        // Abandon an activated owner without cleanup: the kernel state
        // survives while the on-disk claim does not (drop removes it).
        let mut crashed = RouteOwner::default();
        crashed
            .reconcile(&ifcfg, &net_ns, "et-tun", &desired, 0x42)
            .await
            .unwrap();
        let residue_tables = [
            crashed.linux.as_ref().unwrap().table,
            crashed.linux.as_ref().unwrap().blackhole_table,
        ];
        drop(crashed);
        // Restart used to fail permanently: residue rules count as external
        // and leave no adjacent priority pair for the new activation.
        let blocked = RouteOwner::default()
            .reconcile(&ifcfg, &net_ns, "et-tun", &desired, 0x42)
            .await
            .unwrap_err();
        assert!(
            blocked.to_string().contains("safe VPN priorities"),
            "unexpected restart failure: {blocked:#}"
        );
        // The startup sweep reclaims the residue by signature.
        sweep_stale_policy_state(&net_ns, namespace, &lease_dir).await;
        assert!(!policy_rules_reference_any_table(&net_ns, &residue_tables).await);
        assert_output_guard_state(&net_ns, residue_tables[0], false)
            .await
            .unwrap();
        let mut revived = RouteOwner::default();
        revived
            .reconcile(&ifcfg, &net_ns, "et-tun", &desired, 0x42)
            .await
            .unwrap();
        let revived_table = revived.linux.as_ref().unwrap().table;
        // A live lease protects a running instance's state from the sweep.
        sweep_stale_policy_state(&net_ns, namespace, &lease_dir).await;
        assert!(policy_rules_reference_any_table(&net_ns, &[revived_table]).await);
        assert_output_guard_state(&net_ns, revived_table, true)
            .await
            .unwrap();
        // A dead owner's lease turns its state into sweep targets. Drop the
        // owner first, as a killed process would be (its map claim and lease
        // file disappear while the kernel state remains), then reinstall the
        // lease file marked with a foreign boot id to model the crash.
        let lease_path =
            policy_lease_path(&lease_dir, std::process::id(), namespace, revived_table);
        let lease_contents = std::fs::read_to_string(&lease_path).unwrap();
        drop(revived);
        let mut dead: LinuxPolicyLease = serde_json::from_str(&lease_contents).unwrap();
        dead.boot_id = "00000000-0000-0000-0000-000000000000".to_owned();
        std::fs::write(&lease_path, serde_json::to_string(&dead).unwrap()).unwrap();
        sweep_stale_policy_state(&net_ns, namespace, &lease_dir).await;
        assert!(!policy_rules_reference_any_table(&net_ns, &[revived_table]).await);
        assert_output_guard_state(&net_ns, revived_table, false)
            .await
            .unwrap();
        assert!(!lease_path.exists());
    }
}
