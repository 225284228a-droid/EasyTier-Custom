use crate::{
    common::{global_ctx::ArcGlobalCtx, ifcfg::IfConfiguerTrait, netns::NetNS},
    socket_protector::{self, DesktopProtectionLease},
};
use async_trait::async_trait;
use easytier_core::{
    config::vpn::VpnCidr,
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
}

/// Holds the capture set until policy changes or explicit cancellation. Exit
/// liveness never withdraws a route: unavailable exits drop in the packet plane.
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
    #[cfg(target_os = "linux")]
    match (socket_protector::namespace_for(&net_ns), policy_lease_dir()) {
        (Ok(namespace), Ok(dir)) => sweep_stale_policy_state(&net_ns, namespace, &dir).await,
        (namespace, dir) => tracing::warn!(?namespace, ?dir, "VPN policy recovery unavailable"),
    }
    loop {
        let (plan, _) = packet_plane.vpn_route_plan().await;
        // Overlay routes retain their existing NIC/proxy owners on every OS.
        let desired = plan
            .exit_routes
            .into_iter()
            .flat_map(split_default)
            .collect();
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
            activation_error: result.as_ref().err().map(ToString::to_string),
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
    // Keep ownership on failure and retry partial startup/shutdown cleanup.
    // A remaining Linux lease survives for recovery by the next process.
    let mut cleanup = owner
        .reconcile(&ifcfg, &net_ns, &ifname, &BTreeSet::new(), 0)
        .await;
    for _ in 0..2 {
        if cleanup.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cleanup = owner
            .reconcile(&ifcfg, &net_ns, &ifname, &BTreeSet::new(), 0)
            .await;
    }
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
            && self
                .linux
                .as_ref()
                .is_some_and(|table| table.active.iter().all(|active| *active));
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
        if desired.is_empty() && self.linux.is_none() && self.protection.is_none() {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        let lease_dir = policy_lease_dir()?;
        #[cfg(target_os = "linux")]
        let _process_lock =
            linux_namespace_lock(&lease_dir, socket_protector::namespace_for(net_ns)?).await?;
        #[cfg(target_os = "linux")]
        if desired.is_empty() {
            if let Some(table) = self.linux.as_mut() {
                table.release(net_ns).await?;
            }
            self.installed.clear();
            self.pending.clear();
            if let Some(protection) = self.protection.as_mut() {
                protection.release()?;
            }
            self.protection.take();
            self.linux.take();
            return Ok(());
        }
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
                if self.linux.is_none() {
                    self.linux = Some(
                        LinuxPolicyTable::prepare(net_ns, namespace, configured_mark, &lease_dir)
                            .await?,
                    );
                }
                // Retain the table/lease if protecting an existing socket fails.
                let table = self.linux.as_ref().unwrap();
                self.protection = Some(DesktopProtectionLease::acquire(
                    namespace,
                    table.bit,
                    [None, None],
                )?);
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
        #[cfg(not(target_os = "linux"))]
        if desired.is_empty() {
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
            ip(
                net_ns,
                &[
                    family(cidr),
                    "route",
                    "replace",
                    &cidr.to_string(),
                    "dev",
                    ifname,
                    "table",
                    &table.table.to_string(),
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
            let table = self.linux.as_ref().expect("route table owner exists").table;
            if linux_route_exists(
                &policy_routes(net_ns, family(cidr), table).await?,
                cidr,
                ifname,
            ) {
                ip(
                    net_ns,
                    &[
                        family(cidr),
                        "route",
                        "del",
                        &cidr.to_string(),
                        "dev",
                        ifname,
                        "table",
                        &table.to_string(),
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
    priority: [u32; 2],
    bit: u32,
    active: [bool; 2],
    lease_path: PathBuf,
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
        managed_tables()
            .lock()
            .unwrap()
            .remove(&(self.namespace, self.table));
        // Only successful release removes the durable claim. Incomplete owners
        // leave their lease for recovery once the process is gone.
    }
}

#[cfg(target_os = "linux")]
const LINUX_POLICY_TABLE_RANGE: std::ops::Range<u32> = 20000..32766;

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinuxPolicyLease {
    boot_id: String,
    pid: u32,
    namespace: u64,
    table: u32,
    priorities: [u32; 2],
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
fn lease_owner_alive(lease: &LinuxPolicyLease, current_boot_id: &str) -> bool {
    lease.boot_id == current_boot_id && Path::new(&format!("/proc/{}", lease.pid)).exists()
}

#[cfg(all(test, target_os = "linux"))]
tokio::task_local! { static TEST_POLICY_LEASE_DIR: PathBuf; }

#[cfg(target_os = "linux")]
fn policy_lease_dir() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    if let Ok(dir) = TEST_POLICY_LEASE_DIR.try_with(Clone::clone) {
        return Ok(dir);
    }
    if let Some(dir) = std::env::var_os("EASYTIER_VPN_POLICY_LEASE_DIR") {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir)?;
        return Ok(dir);
    }
    let uid = unsafe { libc::geteuid() };
    for dir in [
        PathBuf::from("/run/easytier"),
        std::env::temp_dir().join(format!("easytier-vpn-leases-{uid}")),
    ] {
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let probe = dir.join(format!(".capture-probe-{}", std::process::id()));
        if std::fs::File::create(&probe).is_ok() {
            let _ = std::fs::remove_file(probe);
            return Ok(dir);
        }
    }
    anyhow::bail!("no writable VPN policy lease directory")
}

#[cfg(target_os = "linux")]
fn policy_lease_path(dir: &Path, namespace: u64, table: u32) -> PathBuf {
    dir.join(format!("capture-{namespace}-{table}.json"))
}

#[cfg(target_os = "linux")]
async fn linux_namespace_lock(dir: &Path, namespace: u64) -> anyhow::Result<std::fs::File> {
    let path = dir.join(format!("allocation-{namespace}.lock"));
    tokio::task::spawn_blocking(move || {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // Dropping the descriptor unlocks it. Keep the path: unlinking a lock
        // file could allow new callers to lock a different inode concurrently.
        anyhow::Ok(file)
    })
    .await?
}

#[cfg(target_os = "linux")]
fn reject_foreign_owner(dir: &Path, namespace: u64, boot_id: &str) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("capture-") {
            continue;
        }
        let Ok(contents) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(lease) = serde_json::from_slice::<LinuxPolicyLease>(&contents) else {
            continue;
        };
        anyhow::ensure!(
            lease.namespace != namespace
                || lease.pid == std::process::id()
                || !lease_owner_alive(&lease, boot_id),
            "another process ({}) owns VPN capture in this network namespace",
            lease.pid
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn record_lease(path: &Path, lease: &LinuxPolicyLease) -> anyhow::Result<()> {
    use std::io::Write;
    let contents = serde_json::to_vec(lease)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    // No kernel mutation may precede this complete, durable claim. A crash
    // during the write can therefore leave only an unused incomplete claim.
    if let Err(error) = file.write_all(&contents).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
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
fn policy_rule_matches(rule: &serde_json::Value, table: u32, priority: u32, bit: u32) -> bool {
    rule.get("table").and_then(parse_u32) == Some(table)
        && rule.get("priority").and_then(parse_u32) == Some(priority)
        && rule.get("iif").and_then(serde_json::Value::as_str) == Some("lo")
        && rule.get("fwmark").and_then(parse_u32) == Some(0)
        && rule.get("fwmask").and_then(parse_u32) == Some(bit)
}

#[cfg(target_os = "linux")]
async fn policy_rules(net_ns: &NetNS, family: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    Ok(serde_json::from_slice(
        &ip(net_ns, &[family, "-j", "-N", "rule", "show"]).await?,
    )?)
}

#[cfg(target_os = "linux")]
async fn policy_routes(
    net_ns: &NetNS,
    family: &str,
    table: u32,
) -> anyhow::Result<Vec<serde_json::Value>> {
    // Listing all tables also succeeds after our empty table disappears.
    let routes: Vec<serde_json::Value> = serde_json::from_slice(
        &ip(
            net_ns,
            &[family, "-j", "-N", "route", "show", "table", "all"],
        )
        .await?,
    )?;
    Ok(routes
        .into_iter()
        .filter(|route| route.get("table").and_then(parse_u32) == Some(table))
        .collect())
}

#[cfg(target_os = "linux")]
async fn clear_policy_family(
    net_ns: &NetNS,
    family: &str,
    table: u32,
    priority: u32,
    bit: u32,
) -> anyhow::Result<()> {
    let rules = policy_rules(net_ns, family).await?;
    anyhow::ensure!(
        rules
            .iter()
            .filter(|rule| rule.get("table").and_then(parse_u32) == Some(table))
            .all(|rule| policy_rule_matches(rule, table, priority, bit)),
        "VPN table {table} is referenced by a foreign policy rule"
    );
    let mark = format!("0/{bit:#x}");
    for _ in rules
        .iter()
        .filter(|rule| policy_rule_matches(rule, table, priority, bit))
    {
        ip(
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
        .await?;
    }
    if !policy_routes(net_ns, family, table).await?.is_empty() {
        ip(
            net_ns,
            &[family, "route", "flush", "table", &table.to_string()],
        )
        .await?;
    }
    Ok(())
}

/// Recover only new single-table leases. Upgrades must stop old instances
/// normally; no old guard or unclaimed-signature migration is performed.
#[cfg(target_os = "linux")]
async fn sweep_stale_policy_state(net_ns: &NetNS, namespace: u64, lease_dir: &Path) {
    let _allocation = linux_allocation_gate().lock().await;
    let _process_lock = match linux_namespace_lock(lease_dir, namespace).await {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(?error, "failed to lock VPN policy recovery");
            return;
        }
    };
    let boot_id = linux_boot_id();
    let entries = match std::fs::read_dir(lease_dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(?error, ?lease_dir, "failed to list VPN policy leases");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("capture-") {
            continue;
        }
        let lease = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|contents| Ok(serde_json::from_slice::<LinuxPolicyLease>(&contents)?));
        let lease = match lease {
            Ok(lease) => lease,
            Err(error) => {
                tracing::warn!(?error, ?path, "ignoring invalid VPN policy lease");
                continue;
            }
        };
        if lease.namespace != namespace
            || lease_owner_alive(&lease, &boot_id)
            || managed_tables()
                .lock()
                .unwrap()
                .contains_key(&(namespace, lease.table))
        {
            continue;
        }
        if !LINUX_POLICY_TABLE_RANGE.contains(&lease.table)
            || !lease.bit.is_power_of_two()
            || lease.priorities.contains(&0)
            || path != policy_lease_path(lease_dir, namespace, lease.table)
        {
            tracing::warn!(?path, "ignoring VPN lease with invalid resource identity");
            continue;
        }
        let cleanup = async {
            for (index, family) in ["-4", "-6"].into_iter().enumerate() {
                clear_policy_family(
                    net_ns,
                    family,
                    lease.table,
                    lease.priorities[index],
                    lease.bit,
                )
                .await?;
            }
            std::fs::remove_file(&path)?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = cleanup {
            tracing::warn!(
                ?error,
                ?path,
                "stale VPN policy cleanup failed; keeping lease for retry"
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn select_priority(local: u32, external: u32, occupied: &BTreeSet<u32>) -> anyhow::Result<u32> {
    (local.saturating_add(1)..external).find(|priority| !occupied.contains(priority))
        .ok_or_else(|| anyhow::anyhow!("external policy rules leave no safe VPN priority after local delivery and before external routing"))
}

#[cfg(target_os = "linux")]
impl LinuxPolicyTable {
    async fn prepare(
        net_ns: &NetNS,
        namespace: u64,
        configured_mark: u32,
        dir: &Path,
    ) -> anyhow::Result<Self> {
        let boot_id = linux_boot_id();
        anyhow::ensure!(
            !boot_id.is_empty(),
            "cannot identify the boot owning VPN policy state"
        );
        reject_foreign_owner(dir, namespace, &boot_id)?;
        let managed = managed_tables().lock().unwrap().clone();
        let mut tables = managed
            .keys()
            .filter(|(ns, _)| *ns == namespace)
            .map(|(_, table)| *table)
            .collect::<BTreeSet<_>>();
        let mut masks = Vec::new();
        let mut priority = [0; 2];
        for (index, family) in ["-4", "-6"].into_iter().enumerate() {
            let mut occupied = managed
                .iter()
                .filter(|((ns, _), _)| *ns == namespace)
                .map(|(_, managed)| managed.priority[index])
                .collect::<BTreeSet<_>>();
            let mut local = 0;
            let mut external = u32::MAX;
            for rule in policy_rules(net_ns, family).await? {
                let table = rule.get("table").and_then(parse_u32);
                let pref = rule.get("priority").and_then(parse_u32);
                let owned = table
                    .and_then(|table| managed.get(&(namespace, table)))
                    .is_some_and(|managed| {
                        table.is_some_and(|table| {
                            policy_rule_matches(&rule, table, managed.priority[index], managed.bit)
                        })
                    });
                if !owned {
                    if let Some(mask) = rule.get("fwmask").and_then(parse_u32) {
                        masks.push(mask);
                    } else if rule.get("fwmark").is_some() {
                        masks.push(u32::MAX);
                    }
                }
                if let Some(table) = table {
                    tables.insert(table);
                }
                if let Some(pref) = pref {
                    occupied.insert(pref);
                    if table == Some(255) {
                        local = local.max(pref);
                    } else if !owned {
                        external = external.min(pref);
                    }
                }
            }
            priority[index] = select_priority(local, external, &occupied)?;
            let routes: Vec<serde_json::Value> = serde_json::from_slice(
                &ip(
                    net_ns,
                    &[family, "-j", "-N", "route", "show", "table", "all"],
                )
                .await?,
            )?;
            tables.extend(
                routes
                    .iter()
                    .filter_map(|route| route.get("table").and_then(parse_u32)),
            );
        }
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
        for table in LINUX_POLICY_TABLE_RANGE
            .clone()
            .filter(|table| !tables.contains(table))
        {
            let lease_path = policy_lease_path(dir, namespace, table);
            let lease = LinuxPolicyLease {
                boot_id: boot_id.clone(),
                pid: std::process::id(),
                namespace,
                table,
                priorities: priority,
                bit,
            };
            match record_lease(&lease_path, &lease) {
                Ok(()) => {}
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
            managed_tables()
                .lock()
                .unwrap()
                .insert((namespace, table), ManagedPolicyTable { priority, bit });
            return Ok(Self {
                namespace,
                table,
                priority,
                bit,
                active: [false; 2],
                lease_path,
            });
        }
        anyhow::bail!("no unused dedicated VPN capture routing table")
    }

    async fn activate(&mut self, net_ns: &NetNS) -> anyhow::Result<()> {
        for (index, family) in ["-4", "-6"].into_iter().enumerate() {
            if self.active[index] {
                continue;
            }
            // Avoid duplicate rules when a previous interrupted add succeeded.
            if !policy_rules(net_ns, family)
                .await?
                .iter()
                .any(|rule| policy_rule_matches(rule, self.table, self.priority[index], self.bit))
            {
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
                        &format!("0/{:#x}", self.bit),
                        "lookup",
                        &self.table.to_string(),
                    ],
                )
                .await?;
            }
            self.active[index] = true;
        }
        Ok(())
    }

    async fn release(&mut self, net_ns: &NetNS) -> anyhow::Result<()> {
        for (index, family) in ["-4", "-6"].into_iter().enumerate() {
            clear_policy_family(net_ns, family, self.table, self.priority[index], self.bit).await?;
            self.active[index] = false;
        }
        match std::fs::remove_file(&self.lease_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_route_exists(routes: &[serde_json::Value], destination: VpnCidr, ifname: &str) -> bool {
    routes.iter().any(|route| {
        let matches_destination = route
            .get("dst")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| {
                if value == "default" {
                    return destination.is_default_route();
                }
                value
                    .parse::<VpnCidr>()
                    .ok()
                    .or_else(|| value.parse().ok().map(VpnCidr::host))
                    == Some(destination)
            });
        matches_destination
            && route
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unicast")
                == "unicast"
            && route.get("dev").and_then(serde_json::Value::as_str) == Some(ifname)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_defaults_preserves_the_hosts_physical_defaults() {
        for (cidr, expected) in [
            ("0.0.0.0/0", ["0.0.0.0/1", "128.0.0.0/1"]),
            ("::/0", ["::/1", "8000::/1"]),
        ] {
            assert_eq!(
                split_default(cidr.parse().unwrap())
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                expected
            );
        }
        assert_eq!(
            split_default("203.0.113.0/24".parse().unwrap()),
            vec!["203.0.113.0/24".parse().unwrap()]
        );
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;
        use easytier_core::{
            config::vpn::{VpnConfig, VpnRuleConfig},
            peers::vpn_policy::{VpnDecision, VpnPolicyEngine},
        };

        fn test_lease(boot_id: &str, pid: u32) -> LinuxPolicyLease {
            LinuxPolicyLease {
                boot_id: boot_id.to_owned(),
                pid,
                namespace: 7,
                table: 20000,
                priorities: [1, 1],
                bit: 0x80,
            }
        }

        #[test]
        fn leases_accept_only_the_single_table_format_and_identify_live_owners() {
            let boot_id = linux_boot_id();
            let lease = test_lease(&boot_id, std::process::id());
            let contents = serde_json::to_vec(&lease).unwrap();
            assert_eq!(
                serde_json::from_slice::<LinuxPolicyLease>(&contents).unwrap(),
                lease
            );
            assert!(lease_owner_alive(&lease, &boot_id));
            assert!(!lease_owner_alive(
                &test_lease("old-boot", std::process::id()),
                &boot_id
            ));
            assert!(!lease_owner_alive(
                &test_lease(&boot_id, u32::MAX),
                &boot_id
            ));
            assert!(
                serde_json::from_value::<LinuxPolicyLease>(serde_json::json!({
                    "boot_id": boot_id, "pid": 1, "namespace": 7,
                    "capture_table": 20000, "blackhole_table": 20001,
                    "priorities": [[1, 1], [2, 2]], "bit": 128,
                }))
                .is_err()
            );
        }

        #[test]
        fn allocation_needs_only_one_free_priority_and_skips_other_owners() {
            assert_eq!(select_priority(0, 2, &BTreeSet::from([0, 2])).unwrap(), 1);
            assert_eq!(
                select_priority(0, 4, &BTreeSet::from([0, 1, 2, 4])).unwrap(),
                3
            );
            assert!(select_priority(0, 1, &BTreeSet::new()).is_err());
            assert!(select_priority(u32::MAX, u32::MAX, &BTreeSet::new()).is_err());
        }

        #[tokio::test]
        async fn namespace_file_lock_serializes_independent_file_descriptors() {
            use std::os::fd::AsRawFd;
            let dir = tempfile::tempdir().unwrap();
            let first = linux_namespace_lock(dir.path(), 7).await.unwrap();
            let second = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.path().join("allocation-7.lock"))
                .unwrap();
            assert_ne!(
                unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock
            );
            drop(first);
            assert_eq!(
                unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
        }

        #[test]
        fn live_foreign_process_claims_reject_only_the_same_namespace() {
            let dir = tempfile::tempdir().unwrap();
            let boot_id = linux_boot_id();
            let mut lease = test_lease(&boot_id, unsafe { libc::getppid() } as u32);
            assert_ne!(lease.pid, std::process::id());
            let path = policy_lease_path(dir.path(), lease.namespace, lease.table);
            record_lease(&path, &lease).unwrap();
            let original = std::fs::read(&path).unwrap();
            assert!(record_lease(&path, &lease).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
            assert!(reject_foreign_owner(dir.path(), 7, &boot_id).is_err());
            assert!(reject_foreign_owner(dir.path(), 8, &boot_id).is_ok());
            lease.pid = std::process::id();
            std::fs::write(&path, serde_json::to_vec(&lease).unwrap()).unwrap();
            assert!(reject_foreign_owner(dir.path(), 7, &boot_id).is_ok());
            lease.pid = u32::MAX;
            std::fs::write(&path, serde_json::to_vec(&lease).unwrap()).unwrap();
            assert!(reject_foreign_owner(dir.path(), 7, &boot_id).is_ok());
        }

        #[test]
        fn cleanup_matches_the_owned_rule_and_normalized_unicast_routes() {
            let rule = serde_json::json!({"table": 20000, "priority": 1, "iif": "lo", "fwmark": "0x0", "fwmask": "0x80"});
            assert!(policy_rule_matches(&rule, 20000, 1, 128));
            for (key, value) in [
                ("table", serde_json::json!(20001)),
                ("priority", serde_json::json!(2)),
                ("iif", serde_json::json!("physical")),
                ("fwmark", serde_json::json!(1)),
                ("fwmask", serde_json::json!(255)),
            ] {
                let mut foreign = rule.clone();
                foreign[key] = value;
                assert!(!policy_rule_matches(&foreign, 20000, 1, 128));
            }
            let routes = vec![
                serde_json::json!({"dst": "203.0.113.1", "dev": "et-tun"}),
                serde_json::json!({"dst": "2001:db8:2::/64", "type": "unicast", "dev": "et-tun"}),
                serde_json::json!({"dst": "default", "dev": "et-tun"}),
                serde_json::json!({"dst": "198.51.100.0/24", "type": "unreachable", "dev": "lo"}),
            ];
            assert!(linux_route_exists(
                &routes,
                "203.0.113.1/32".parse().unwrap(),
                "et-tun"
            ));
            assert!(linux_route_exists(
                &routes,
                "2001:db8:2::/64".parse().unwrap(),
                "et-tun"
            ));
            assert!(linux_route_exists(
                &routes,
                "0.0.0.0/0".parse().unwrap(),
                "et-tun"
            ));
            assert!(!linux_route_exists(
                &routes,
                "203.0.113.1/32".parse().unwrap(),
                "physical"
            ));
            assert!(!linux_route_exists(
                &routes,
                "198.51.100.0/24".parse().unwrap(),
                "lo"
            ));
        }

        struct KernelEnvironment {
            net_ns: NetNS,
            peer_ns: NetNS,
            client_ns: NetNS,
            names: Vec<String>,
            lease_dir: PathBuf,
        }
        impl Drop for KernelEnvironment {
            fn drop(&mut self) {
                for name in &self.names {
                    let _ = std::process::Command::new("ip")
                        .args(["netns", "del", name])
                        .output();
                }
                let _ = std::fs::remove_dir_all(&self.lease_dir);
            }
        }
        impl KernelEnvironment {
            async fn new(label: &str) -> Self {
                let name = format!(
                    "et-vpn-{label}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                );
                let peer = format!("{name}-peer");
                let client = format!("{name}-client");
                let lease_dir = std::env::temp_dir().join(format!("{name}-leases"));
                std::fs::create_dir_all(&lease_dir).unwrap();
                let mut environment = Self {
                    net_ns: NetNS::new(Some(name.clone())),
                    peer_ns: NetNS::new(Some(peer.clone())),
                    client_ns: NetNS::new(Some(client.clone())),
                    names: Vec::new(),
                    lease_dir,
                };
                for name in [&name, &peer, &client] {
                    let result = tokio::process::Command::new("ip")
                        .args(["netns", "add", name])
                        .output()
                        .await
                        .unwrap();
                    assert!(
                        result.status.success(),
                        "create isolated namespace: {}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                    environment.names.push(name.clone());
                }
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
                    vec!["link", "set", "physical-peer", "netns", &peer],
                    vec!["link", "set", "physical", "up"],
                    vec!["addr", "add", "192.0.2.2/24", "dev", "physical"],
                    vec![
                        "-6",
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
                        "-6",
                        "route",
                        "add",
                        "default",
                        "via",
                        "2001:db8:1::1",
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
                    vec!["link", "set", "client-tun", "netns", &client],
                    vec!["link", "set", "et-tun", "up"],
                    vec!["addr", "add", "10.66.0.1/24", "dev", "et-tun"],
                    vec!["-6", "addr", "add", "fd66::1/64", "dev", "et-tun", "nodad"],
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
                ] {
                    ip(&environment.net_ns, &args).await.unwrap();
                }
                for args in [
                    vec!["link", "set", "lo", "up"],
                    vec!["link", "set", "physical-peer", "up"],
                    vec!["addr", "add", "192.0.2.1/24", "dev", "physical-peer"],
                    vec![
                        "-6",
                        "addr",
                        "add",
                        "2001:db8:1::1/64",
                        "dev",
                        "physical-peer",
                        "nodad",
                    ],
                    vec!["addr", "add", "203.0.113.1/32", "dev", "lo"],
                    vec![
                        "-6",
                        "addr",
                        "add",
                        "2001:db8:2::1/128",
                        "dev",
                        "lo",
                        "nodad",
                    ],
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
                    ip(&environment.peer_ns, &args).await.unwrap();
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
                    ip(&environment.client_ns, &args).await.unwrap();
                }
                environment.net_ns.run(|| {
                    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1").unwrap();
                    for interface in ["all", "default", "physical", "et-tun"] {
                        std::fs::write(
                            format!("/proc/sys/net/ipv4/conf/{interface}/rp_filter"),
                            "0",
                        )
                        .unwrap();
                    }
                });
                environment
            }
        }

        async fn assert_lookup(net_ns: &NetNS, args: &[&str], expected: &str) {
            let route: Vec<serde_json::Value> =
                serde_json::from_slice(&ip(net_ns, args).await.unwrap()).unwrap();
            assert_eq!(route[0]["dev"], expected);
        }
        async fn assert_table_empty(net_ns: &NetNS, table: u32) {
            for family in ["-4", "-6"] {
                assert!(
                    policy_rules(net_ns, family)
                        .await
                        .unwrap()
                        .iter()
                        .all(|rule| rule.get("table").and_then(parse_u32) != Some(table))
                );
                assert!(
                    policy_routes(net_ns, family, table)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
        fn enabled() -> bool {
            std::env::var_os("EASYTIER_RUN_VPN_ROUTE_KERNEL_TESTS").is_some()
        }

        fn capture_lease_count(dir: &Path) -> usize {
            std::fs::read_dir(dir)
                .unwrap()
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("capture-"))
                .count()
        }

        /// Every network mutation occurs in throwaway namespaces. Requires
        /// CAP_SYS_ADMIN/CAP_NET_ADMIN and never changes host routing/firewalls.
        #[tokio::test]
        async fn kernel_capture_bypass_outage_forwarding_hot_update_and_independent_owners() {
            if !enabled() {
                return;
            }
            let environment = KernelEnvironment::new("basic").await;
            TEST_POLICY_LEASE_DIR.scope(environment.lease_dir.clone(), async {
                let net_ns = &environment.net_ns;
                let ifcfg: BoxedIfCfg = Arc::new(crate::common::ifcfg::IfConfiger {});
                let server = environment.peer_ns.run(|| std::net::UdpSocket::bind("203.0.113.1:0")).unwrap();
                server.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
                let remote = server.local_addr().unwrap();
                let existing = net_ns.run(|| socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))).unwrap();
                existing.set_mark(0x42).unwrap();
                existing.bind(&"0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap().into()).unwrap();
                let _existing_registration = socket_protector::protect_native_socket(&existing, true).await.unwrap();
                let exit = "10.66.0.2".parse().unwrap();
                let engine = VpnPolicyEngine::new(VpnConfig { exit_enabled: false, rules: vec![
                    VpnRuleConfig { cidr: "203.0.113.0/24".parse().unwrap(), exit },
                    VpnRuleConfig { cidr: "2001:db8:2::/64".parse().unwrap(), exit },
                ] }).unwrap();
                let desired = engine.route_plan().0.exit_routes.into_iter().flat_map(split_default).collect::<BTreeSet<_>>();
                let mut owner = RouteOwner::default();
                owner.reconcile(&ifcfg, net_ns, "et-tun", &desired, 0x42).await.unwrap();
                let table = owner.linux.as_ref().unwrap().table;
                let bit = owner.linux.as_ref().unwrap().bit;
                assert_eq!(existing.mark().unwrap(), 0x42 | bit);
                assert!(owner.active());
                for (index, family) in ["-4", "-6"].into_iter().enumerate() {
                    let rules = policy_rules(net_ns, family).await.unwrap();
                    assert_eq!(rules.iter().filter(|rule| rule.get("table").and_then(parse_u32) == Some(table)).count(), 1);
                    assert!(rules.iter().any(|rule| policy_rule_matches(rule, table, owner.linux.as_ref().unwrap().priority[index], bit)));
                }
                assert_lookup(net_ns, &["-4", "-j", "route", "get", "203.0.113.1"], "et-tun").await;
                assert_lookup(net_ns, &["-6", "-j", "route", "get", "2001:db8:2::1"], "et-tun").await;
                assert_lookup(net_ns, &["-4", "-j", "route", "get", "10.66.0.1"], "lo").await;
                assert_lookup(net_ns, &["-4", "-j", "route", "get", "203.0.113.1", "mark", &format!("{:#x}", 0x42 | bit)], "physical").await;
                assert_lookup(net_ns, &["-4", "-j", "route", "get", "203.0.113.1", "from", "10.66.0.2", "iif", "et-tun"], "physical").await;
                let mut received = [0; 64];
                existing.send_to(b"existing protected socket", &remote.into()).unwrap();
                let (length, source) = server.recv_from(&mut received).unwrap();
                assert_eq!(&received[..length], b"existing protected socket");
                assert_eq!(source.ip(), "192.0.2.2".parse::<std::net::IpAddr>().unwrap());
                let fresh = net_ns.run(|| socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))).unwrap();
                let _fresh_registration = socket_protector::protect_native_socket(&fresh, true).await.unwrap();
                fresh.send_to(b"new protected socket", &remote.into()).unwrap();
                let (length, _) = server.recv_from(&mut received).unwrap();
                assert_eq!(&received[..length], b"new protected socket");
                let forwarded = environment.client_ns.run(|| std::net::UdpSocket::bind("10.66.0.2:0")).unwrap();
                forwarded.send_to(b"forwarded exit packet", remote).unwrap();
                let (length, source) = server.recv_from(&mut received).unwrap();
                assert_eq!(&received[..length], b"forwarded exit packet");
                assert_eq!(source.ip(), exit);
                engine.update_exit_states([(exit, Some(42))]);
                assert_eq!(engine.resolve(&remote.ip()), VpnDecision::Exit(42));
                engine.update_exit_states([(exit, None)]);
                assert_eq!(engine.resolve(&remote.ip()), VpnDecision::Drop);
                owner.reconcile(&ifcfg, net_ns, "et-tun", &desired, 0x42).await.unwrap();
                assert_lookup(net_ns, &["-4", "-j", "route", "get", "203.0.113.1"], "et-tun").await;
                let ordinary = net_ns.run(|| std::net::UdpSocket::bind("0.0.0.0:0")).unwrap();
                ordinary.send_to(b"captured during outage", remote).unwrap();
                assert!(matches!(server.recv_from(&mut received), Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)));
                let defaults = ["0.0.0.0/0", "::/0"].into_iter().flat_map(|cidr| split_default(cidr.parse().unwrap())).collect();
                owner.reconcile(&ifcfg, net_ns, "et-tun", &defaults, 0x42).await.unwrap();
                assert_lookup(net_ns, &["-4", "-j", "route", "get", "198.51.100.1"], "et-tun").await;
                assert_lookup(net_ns, &["-6", "-j", "route", "get", "2001:db8:3::1"], "et-tun").await;
                owner.reconcile(&ifcfg, net_ns, "et-tun", &desired, 0x42).await.unwrap();
                for args in [vec!["link", "add", "et-other", "type", "dummy"], vec!["link", "set", "et-other", "up"],
                    vec!["addr", "add", "10.67.0.1/24", "dev", "et-other"]] { ip(net_ns, &args).await.unwrap(); }
                let mut second = RouteOwner::default();
                second.reconcile(&ifcfg, net_ns, "et-other", &BTreeSet::from(["198.51.100.0/24".parse().unwrap()]), 0x42).await.unwrap();
                let second_table = second.linux.as_ref().unwrap().table;
                assert_ne!(table, second_table);
                assert_ne!(owner.linux.as_ref().unwrap().priority, second.linux.as_ref().unwrap().priority);
                assert_eq!(bit, second.linux.as_ref().unwrap().bit);
                sweep_stale_policy_state(net_ns, socket_protector::namespace_for(net_ns).unwrap(), &environment.lease_dir).await;
                assert_lookup(net_ns, &["-4", "-j", "route", "get", "198.51.100.1"], "et-other").await;
                owner.reconcile(&ifcfg, net_ns, "et-tun", &BTreeSet::new(), 0).await.unwrap();
                assert_table_empty(net_ns, table).await;
                assert_eq!(existing.mark().unwrap(), 0x42 | bit);
                assert_lookup(net_ns, &["-4", "-j", "route", "get", "198.51.100.1"], "et-other").await;
                second.reconcile(&ifcfg, net_ns, "et-other", &BTreeSet::new(), 0).await.unwrap();
                assert_table_empty(net_ns, second_table).await;
                assert_eq!(existing.mark().unwrap(), 0x42);
                assert_lookup(net_ns, &["-4", "-j", "route", "get", "203.0.113.1"], "physical").await;
                assert_eq!(capture_lease_count(&environment.lease_dir), 0);
            }).await;
        }

        #[tokio::test]
        async fn kernel_partial_startup_and_cleanup_retry_and_dead_lease_recovery() {
            if !enabled() {
                return;
            }
            let environment = KernelEnvironment::new("recovery").await;
            TEST_POLICY_LEASE_DIR
                .scope(environment.lease_dir.clone(), async {
                    let net_ns = &environment.net_ns;
                    let namespace = socket_protector::namespace_for(net_ns).unwrap();
                    let ifcfg: BoxedIfCfg = Arc::new(crate::common::ifcfg::IfConfiger {});
                    net_ns
                        .run(|| std::fs::write("/proc/sys/net/ipv6/conf/et-tun/disable_ipv6", "1"))
                        .unwrap();
                    let desired = BTreeSet::from([
                        "203.0.113.0/24".parse().unwrap(),
                        "2001:db8:2::/64".parse().unwrap(),
                    ]);
                    let mut partial = RouteOwner::default();
                    assert!(
                        partial
                            .reconcile(&ifcfg, net_ns, "et-tun", &desired, 0x42)
                            .await
                            .is_err()
                    );
                    assert_eq!(
                        partial.installed.len(),
                        1,
                        "IPv4 must complete before the forced IPv6 failure"
                    );
                    assert_eq!(partial.pending.len(), 1);
                    let table = partial.linux.as_ref().unwrap().table;
                    let path = partial.linux.as_ref().unwrap().lease_path.clone();
                    assert!(path.exists());
                    assert!(!policy_routes(net_ns, "-4", table).await.unwrap().is_empty());
                    assert!(
                        policy_rules(net_ns, "-4")
                            .await
                            .unwrap()
                            .iter()
                            .all(|rule| rule.get("table").and_then(parse_u32) != Some(table))
                    );
                    ip(
                        net_ns,
                        &[
                            "-4",
                            "rule",
                            "add",
                            "priority",
                            "105",
                            "lookup",
                            &table.to_string(),
                        ],
                    )
                    .await
                    .unwrap();
                    assert!(
                        partial
                            .reconcile(&ifcfg, net_ns, "et-tun", &BTreeSet::new(), 0)
                            .await
                            .is_err()
                    );
                    assert!(
                        path.exists()
                            && partial.protection.is_some()
                            && !partial.installed.is_empty()
                    );
                    ip(
                        net_ns,
                        &[
                            "-4",
                            "rule",
                            "del",
                            "priority",
                            "105",
                            "lookup",
                            &table.to_string(),
                        ],
                    )
                    .await
                    .unwrap();
                    partial
                        .reconcile(&ifcfg, net_ns, "et-tun", &BTreeSet::new(), 0)
                        .await
                        .unwrap();
                    partial
                        .reconcile(&ifcfg, net_ns, "et-tun", &BTreeSet::new(), 0)
                        .await
                        .unwrap();
                    assert_table_empty(net_ns, table).await;
                    assert!(!path.exists());
                    assert!(socket_protector::active_bit(namespace).is_none());
                    let mut crashed = RouteOwner::default();
                    let ipv4 = BTreeSet::from(["203.0.113.0/24".parse().unwrap()]);
                    crashed
                        .reconcile(&ifcfg, net_ns, "et-tun", &ipv4, 0x42)
                        .await
                        .unwrap();
                    let table = crashed.linux.as_ref().unwrap().table;
                    let path = crashed.linux.as_ref().unwrap().lease_path.clone();
                    sweep_stale_policy_state(net_ns, namespace, &environment.lease_dir).await;
                    assert!(path.exists());
                    assert_lookup(
                        net_ns,
                        &["-4", "-j", "route", "get", "203.0.113.1"],
                        "et-tun",
                    )
                    .await;
                    let mut lease: LinuxPolicyLease =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    // Model a killed process: socket registry ownership disappears,
                    // the durable lease and kernel resources remain.
                    crashed.protection.take().unwrap().release().unwrap();
                    drop(crashed);
                    lease.boot_id = "dead-test-boot".to_owned();
                    std::fs::write(&path, serde_json::to_vec(&lease).unwrap()).unwrap();
                    ip(
                        net_ns,
                        &[
                            "-6",
                            "rule",
                            "add",
                            "priority",
                            "105",
                            "lookup",
                            &table.to_string(),
                        ],
                    )
                    .await
                    .unwrap();
                    sweep_stale_policy_state(net_ns, namespace, &environment.lease_dir).await;
                    assert!(path.exists(), "incomplete recovery must retain its claim");
                    assert!(policy_routes(net_ns, "-4", table).await.unwrap().is_empty());
                    assert!(
                        policy_rules(net_ns, "-6")
                            .await
                            .unwrap()
                            .iter()
                            .any(|rule| rule.get("priority").and_then(parse_u32) == Some(105))
                    );
                    ip(
                        net_ns,
                        &[
                            "-6",
                            "rule",
                            "del",
                            "priority",
                            "105",
                            "lookup",
                            &table.to_string(),
                        ],
                    )
                    .await
                    .unwrap();
                    sweep_stale_policy_state(net_ns, namespace, &environment.lease_dir).await;
                    assert!(!path.exists());
                    assert_table_empty(net_ns, table).await;
                    let mut revived = RouteOwner::default();
                    revived
                        .reconcile(&ifcfg, net_ns, "et-tun", &ipv4, 0x42)
                        .await
                        .unwrap();
                    assert!(revived.active());
                    revived
                        .reconcile(&ifcfg, net_ns, "et-tun", &BTreeSet::new(), 0)
                        .await
                        .unwrap();
                    assert_eq!(capture_lease_count(&environment.lease_dir), 0);
                })
                .await;
        }
    }
}
