//! Owned exit-side platform setup for the multi-exit VPN.
//!
//! IPv4 exit traffic is always NATed by the userspace proxy engines. Linux
//! IPv6 exit traffic uses kernel forwarding and an ip6tables MASQUERADE rule.
//! The lease manager shares those settings between instances and restores the
//! values that were present before the first lease was acquired.

use crate::common::netns::NetNS;
use anyhow::Result;
#[cfg(any(target_os = "linux", test))]
use anyhow::{Context, anyhow};
#[cfg(any(target_os = "linux", test))]
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "linux")]
static NEXT_LEASE_ID: AtomicU64 = AtomicU64::new(1);

/// Owns this instance's IPv6 exit-forwarding setup.
///
/// Call [`reconcile`](Self::reconcile) when the exit state changes and
/// [`release`](Self::release) during shutdown. Cleanup errors leave the lease
/// in place so callers can retry.
pub struct VpnExitForwardingLease {
    net_ns: NetNS,
    #[cfg(target_os = "linux")]
    owner_id: u64,
    #[cfg(target_os = "linux")]
    active: Option<ActiveLease>,
}

impl VpnExitForwardingLease {
    pub fn new(net_ns: NetNS) -> Self {
        Self {
            net_ns,
            #[cfg(target_os = "linux")]
            owner_id: NEXT_LEASE_ID.fetch_add(1, Ordering::Relaxed),
            #[cfg(target_os = "linux")]
            active: None,
        }
    }

    /// Acquire a lease when an IPv6 prefix is configured.
    ///
    /// A missing prefix does not enter the namespace or change kernel state.
    pub async fn acquire(
        net_ns: NetNS,
        ifname: &str,
        prefix: Option<String>,
    ) -> Result<Option<Self>> {
        let Some(prefix) = prefix else {
            return Ok(None);
        };

        #[cfg(target_os = "linux")]
        {
            let mut lease = Self::new(net_ns);
            lease.reconcile(ifname, Some(prefix), true).await?;
            Ok(Some(lease))
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (net_ns, ifname, prefix);
            Ok(None)
        }
    }

    /// Update the leased interface and prefix, or release them when disabled.
    pub async fn reconcile(
        &mut self,
        ifname: &str,
        prefix: Option<String>,
        enabled: bool,
    ) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.reconcile_linux(ifname, prefix, enabled).await
        }

        #[cfg(not(target_os = "linux"))]
        {
            if enabled && prefix.is_some() {
                tracing::warn!(
                    "ipv6 exit forwarding has no platform support here; ipv4 exit traffic still works"
                );
            }
            let _ = (&self.net_ns, ifname);
            Ok(())
        }
    }

    /// Release kernel state owned by this lease.
    pub async fn release(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.release_linux().await
        }

        #[cfg(not(target_os = "linux"))]
        {
            Ok(())
        }
    }

    pub fn prefix(&self) -> Option<&str> {
        #[cfg(target_os = "linux")]
        {
            self.active
                .as_ref()
                .map(|active| active.rule.prefix.as_str())
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    pub fn ifname(&self) -> Option<&str> {
        #[cfg(target_os = "linux")]
        {
            self.active
                .as_ref()
                .map(|active| active.rule.ifname.as_str())
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    #[cfg(target_os = "linux")]
    async fn reconcile_linux(
        &mut self,
        ifname: &str,
        prefix: Option<String>,
        enabled: bool,
    ) -> Result<()> {
        let desired = enabled.then_some(prefix).flatten().map(|prefix| NatRule {
            prefix,
            ifname: ifname.to_owned(),
        });

        if let (Some(active), Some(desired)) = (&self.active, &desired)
            && active.rule == *desired
            && active.is_fully_held()
        {
            let namespace = current_namespace_identity(self.net_ns.clone()).await?;
            if namespace == active.namespace {
                return Ok(());
            }
        }

        if self.active.is_some() {
            self.release_linux().await?;
        }

        let Some(rule) = desired else {
            return Ok(());
        };

        let namespace =
            acquire_in_namespace(self.net_ns.clone(), self.owner_id, rule.clone()).await?;

        self.active = Some(ActiveLease {
            owner_id: self.owner_id,
            namespace,
            rule,
            nat_held: true,
            forwarding_held: true,
        });
        Ok(())
    }

    #[cfg(target_os = "linux")]
    async fn release_linux(&mut self) -> Result<()> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        let backup = active.clone();
        let net_ns = self.net_ns.clone();

        let outcome = tokio::task::spawn_blocking(move || {
            net_ns.run(|| {
                let namespace = linux_namespace_identity()?;
                let mut active = active;
                let result = if namespace != active.namespace {
                    Err(anyhow!(
                        "network namespace identity changed while IPv6 forwarding lease was active"
                    ))
                } else {
                    let mut manager = lease_manager()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    manager.release(&mut active, &mut LinuxOps)
                };
                Ok((active, result))
            })
        })
        .await;

        match outcome {
            Ok(Ok((active, result))) => {
                self.active = active.has_resources().then_some(active);
                result
            }
            Ok(Err(error)) => {
                self.active = Some(backup);
                Err(error)
            }
            Err(error) => {
                self.active = Some(backup);
                Err(anyhow!(error).context("IPv6 forwarding cleanup worker failed"))
            }
        }
    }
}

impl Drop for VpnExitForwardingLease {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let mut manager = lease_manager()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            manager.abandon_owner(self.owner_id);
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg(any(target_os = "linux", test))]
struct NamespaceIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg(any(target_os = "linux", test))]
struct NatRule {
    prefix: String,
    ifname: String,
}

#[derive(Clone)]
#[cfg(any(target_os = "linux", test))]
struct ActiveLease {
    owner_id: u64,
    namespace: NamespaceIdentity,
    rule: NatRule,
    nat_held: bool,
    forwarding_held: bool,
}

#[cfg(any(target_os = "linux", test))]
impl ActiveLease {
    #[cfg(target_os = "linux")]
    fn is_fully_held(&self) -> bool {
        self.nat_held && self.forwarding_held
    }

    fn has_resources(&self) -> bool {
        self.nat_held || self.forwarding_held
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(any(target_os = "linux", test))]
struct ForwardingSnapshot {
    values: BTreeMap<String, Vec<u8>>,
}

#[cfg(any(target_os = "linux", test))]
trait PlatformOps {
    fn snapshot_forwarding(&mut self, namespace: &NamespaceIdentity) -> Result<ForwardingSnapshot>;
    fn enable_forwarding(&mut self, namespace: &NamespaceIdentity) -> Result<()>;
    fn restore_forwarding(
        &mut self,
        namespace: &NamespaceIdentity,
        snapshot: &ForwardingSnapshot,
    ) -> Result<()>;
    fn rule_exists(&mut self, namespace: &NamespaceIdentity, rule: &NatRule) -> Result<bool>;
    fn add_rule(&mut self, namespace: &NamespaceIdentity, rule: &NatRule) -> Result<()>;
    fn delete_rule(&mut self, namespace: &NamespaceIdentity, rule: &NatRule) -> Result<()>;
}

#[derive(Default)]
#[cfg(any(target_os = "linux", test))]
struct LeaseManager {
    forwarding: HashMap<NamespaceIdentity, ForwardingOwners>,
    nat_rules: HashMap<RuleIdentity, NatOwners>,
}

#[cfg(any(target_os = "linux", test))]
struct ForwardingOwners {
    baseline: ForwardingSnapshot,
    owners: HashSet<u64>,
}

#[cfg(any(target_os = "linux", test))]
struct NatOwners {
    owned_by_us: bool,
    owners: HashSet<u64>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg(any(target_os = "linux", test))]
struct RuleIdentity {
    namespace: NamespaceIdentity,
    rule: NatRule,
}

#[cfg(any(target_os = "linux", test))]
impl LeaseManager {
    fn acquire(
        &mut self,
        namespace: &NamespaceIdentity,
        owner_id: u64,
        rule: &NatRule,
        ops: &mut impl PlatformOps,
    ) -> Result<()> {
        self.cleanup_pending_nat(namespace, ops)?;
        self.acquire_forwarding(namespace, owner_id, ops)?;

        if let Err(rule_error) = self.acquire_nat(namespace, owner_id, rule, ops) {
            if let Err(rollback_error) = self.release_forwarding(namespace, owner_id, ops) {
                if let Some(state) = self.forwarding.get_mut(namespace) {
                    state.owners.remove(&owner_id);
                }
                return Err(anyhow!(
                    "failed to install IPv6 MASQUERADE rule: {rule_error:#}; \
                     failed to roll back forwarding: {rollback_error:#}"
                ));
            }
            return Err(rule_error.context("failed to install IPv6 MASQUERADE rule"));
        }

        Ok(())
    }

    fn abandon_owner(&mut self, owner_id: u64) {
        for state in self.forwarding.values_mut() {
            state.owners.remove(&owner_id);
        }

        let identities = self
            .nat_rules
            .iter_mut()
            .filter_map(|(identity, state)| {
                if !state.owners.remove(&owner_id) {
                    return None;
                }
                (state.owners.is_empty() && !state.owned_by_us).then_some(identity.clone())
            })
            .collect::<Vec<_>>();
        for identity in identities {
            self.nat_rules.remove(&identity);
        }
    }

    fn cleanup_pending_nat(
        &mut self,
        namespace: &NamespaceIdentity,
        ops: &mut impl PlatformOps,
    ) -> Result<()> {
        let pending = self
            .nat_rules
            .iter()
            .filter_map(|(identity, state)| {
                (identity.namespace == *namespace && state.owners.is_empty())
                    .then_some((identity.clone(), state.owned_by_us))
            })
            .collect::<Vec<_>>();

        for (identity, owned_by_us) in pending {
            if owned_by_us && ops.rule_exists(namespace, &identity.rule)? {
                ops.delete_rule(namespace, &identity.rule)
                    .context("retrying removal of pending IPv6 MASQUERADE rule")?;
            }
            self.nat_rules.remove(&identity);
        }
        Ok(())
    }

    fn release(&mut self, active: &mut ActiveLease, ops: &mut impl PlatformOps) -> Result<()> {
        if active.nat_held {
            self.release_nat(&active.namespace, active.owner_id, &active.rule, ops)?;
            active.nat_held = false;
        }
        if active.forwarding_held {
            self.release_forwarding(&active.namespace, active.owner_id, ops)?;
            active.forwarding_held = false;
        }
        Ok(())
    }

    fn acquire_forwarding(
        &mut self,
        namespace: &NamespaceIdentity,
        owner_id: u64,
        ops: &mut impl PlatformOps,
    ) -> Result<()> {
        if let Some(state) = self.forwarding.get_mut(namespace) {
            if state.owners.contains(&owner_id) {
                return Ok(());
            }
            if !state.owners.is_empty() {
                state.owners.insert(owner_id);
                return Ok(());
            }

            let previous = state.baseline.clone();
            ops.restore_forwarding(namespace, &previous)
                .context("retrying restoration of prior IPv6 forwarding state")?;
            self.forwarding.remove(namespace);
        }

        let baseline = ops
            .snapshot_forwarding(namespace)
            .context("snapshotting IPv6 forwarding state")?;
        if let Err(enable_error) = ops.enable_forwarding(namespace) {
            if let Err(restore_error) = ops.restore_forwarding(namespace, &baseline) {
                self.forwarding.insert(
                    namespace.clone(),
                    ForwardingOwners {
                        baseline,
                        owners: HashSet::new(),
                    },
                );
                return Err(anyhow!(
                    "failed to enable IPv6 forwarding: {enable_error:#}; \
                     rollback failed and will be retried: {restore_error:#}"
                ));
            }
            return Err(enable_error.context("enabling IPv6 forwarding"));
        }

        self.forwarding.insert(
            namespace.clone(),
            ForwardingOwners {
                baseline,
                owners: HashSet::from([owner_id]),
            },
        );
        Ok(())
    }

    fn release_forwarding(
        &mut self,
        namespace: &NamespaceIdentity,
        owner_id: u64,
        ops: &mut impl PlatformOps,
    ) -> Result<()> {
        let Some(state) = self.forwarding.get(namespace) else {
            return Ok(());
        };
        if !state.owners.contains(&owner_id) {
            return Ok(());
        }
        if state.owners.len() > 1 {
            if let Some(state) = self.forwarding.get_mut(namespace) {
                state.owners.remove(&owner_id);
            }
            return Ok(());
        }

        let baseline = state.baseline.clone();
        ops.restore_forwarding(namespace, &baseline)
            .context("restoring IPv6 forwarding state")?;
        self.forwarding.remove(namespace);
        Ok(())
    }

    fn acquire_nat(
        &mut self,
        namespace: &NamespaceIdentity,
        owner_id: u64,
        rule: &NatRule,
        ops: &mut impl PlatformOps,
    ) -> Result<()> {
        let identity = RuleIdentity {
            namespace: namespace.clone(),
            rule: rule.clone(),
        };

        if let Some(state) = self.nat_rules.get_mut(&identity) {
            if state.owners.contains(&owner_id) {
                return Ok(());
            }
            if !state.owners.is_empty() {
                state.owners.insert(owner_id);
                return Ok(());
            }

            let already_present = ops.rule_exists(namespace, rule)?;
            if !already_present {
                ops.add_rule(namespace, rule)?;
            }
            state.owned_by_us = state.owned_by_us || !already_present;
            state.owners.insert(owner_id);
            return Ok(());
        }

        let already_present = ops.rule_exists(namespace, rule)?;
        if !already_present {
            ops.add_rule(namespace, rule)?;
        }
        self.nat_rules.insert(
            identity,
            NatOwners {
                owned_by_us: !already_present,
                owners: HashSet::from([owner_id]),
            },
        );
        Ok(())
    }

    fn release_nat(
        &mut self,
        namespace: &NamespaceIdentity,
        owner_id: u64,
        rule: &NatRule,
        ops: &mut impl PlatformOps,
    ) -> Result<()> {
        let identity = RuleIdentity {
            namespace: namespace.clone(),
            rule: rule.clone(),
        };
        let Some(state) = self.nat_rules.get(&identity) else {
            return Ok(());
        };
        if !state.owners.contains(&owner_id) {
            return Ok(());
        }
        if state.owners.len() > 1 {
            if let Some(state) = self.nat_rules.get_mut(&identity) {
                state.owners.remove(&owner_id);
            }
            return Ok(());
        }

        if state.owned_by_us && ops.rule_exists(namespace, rule)? {
            ops.delete_rule(namespace, rule)
                .context("removing owned IPv6 MASQUERADE rule")?;
        }
        self.nat_rules.remove(&identity);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn lease_manager() -> &'static std::sync::Mutex<LeaseManager> {
    static MANAGER: std::sync::OnceLock<std::sync::Mutex<LeaseManager>> =
        std::sync::OnceLock::new();
    MANAGER.get_or_init(|| std::sync::Mutex::new(LeaseManager::default()))
}

#[cfg(target_os = "linux")]
struct LinuxOps;

#[cfg(target_os = "linux")]
impl PlatformOps for LinuxOps {
    fn snapshot_forwarding(
        &mut self,
        _namespace: &NamespaceIdentity,
    ) -> Result<ForwardingSnapshot> {
        let root = std::path::Path::new("/proc/sys/net/ipv6/conf");
        let entries =
            std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))?;
        let mut directories = entries
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<Vec<_>>>()?;
        directories.sort();

        let mut snapshot = ForwardingSnapshot::default();
        for directory in directories {
            for field in ["forwarding", "force_forwarding"] {
                let relative = format!("{directory}/{field}");
                match std::fs::read(root.join(&relative)) {
                    Ok(value) => {
                        snapshot.values.insert(relative, value);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("reading IPv6 sysctl {relative}"));
                    }
                }
            }
        }

        for required in ["all/forwarding", "default/forwarding"] {
            if !snapshot.values.contains_key(required) {
                return Err(anyhow!("required IPv6 sysctl {required} is unavailable"));
            }
        }
        Ok(snapshot)
    }

    fn enable_forwarding(&mut self, _namespace: &NamespaceIdentity) -> Result<()> {
        for path in [
            "/proc/sys/net/ipv6/conf/all/forwarding",
            "/proc/sys/net/ipv6/conf/default/forwarding",
        ] {
            std::fs::write(path, b"1").with_context(|| format!("writing {path}"))?;
        }
        Ok(())
    }

    fn restore_forwarding(
        &mut self,
        _namespace: &NamespaceIdentity,
        snapshot: &ForwardingSnapshot,
    ) -> Result<()> {
        let root = std::path::Path::new("/proc/sys/net/ipv6/conf");
        let mut failures = Vec::new();
        for relative in forwarding_restore_order(snapshot) {
            let Some(value) = snapshot.values.get(relative) else {
                continue;
            };
            if let Err(error) = std::fs::write(root.join(relative), value)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                failures.push(format!("{relative}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "failed restoring IPv6 forwarding sysctls: {}",
                failures.join("; ")
            ))
        }
    }

    fn rule_exists(&mut self, _namespace: &NamespaceIdentity, rule: &NatRule) -> Result<bool> {
        let output = rule_command("-C", rule)
            .output()
            .context("running ip6tables to check an IPv6 MASQUERADE rule")?;
        if output.status.success() {
            return Ok(true);
        }
        if output.status.code() == Some(1) {
            return Ok(false);
        }
        Err(command_error("checking IPv6 MASQUERADE rule", output))
    }

    fn add_rule(&mut self, _namespace: &NamespaceIdentity, rule: &NatRule) -> Result<()> {
        let output = rule_command("-A", rule)
            .output()
            .context("running ip6tables to add an IPv6 MASQUERADE rule")?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_error("adding IPv6 MASQUERADE rule", output))
        }
    }

    fn delete_rule(&mut self, _namespace: &NamespaceIdentity, rule: &NatRule) -> Result<()> {
        let output = rule_command("-D", rule)
            .output()
            .context("running ip6tables to remove an IPv6 MASQUERADE rule")?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_error("removing IPv6 MASQUERADE rule", output))
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn forwarding_restore_order(snapshot: &ForwardingSnapshot) -> Vec<&str> {
    let mut order = Vec::with_capacity(snapshot.values.len());
    for path in [
        "all/forwarding",
        "default/forwarding",
        "all/force_forwarding",
        "default/force_forwarding",
    ] {
        if snapshot.values.contains_key(path) {
            order.push(path);
        }
    }
    order.extend(snapshot.values.keys().filter_map(|path| {
        let special = matches!(
            path.as_str(),
            "all/forwarding"
                | "default/forwarding"
                | "all/force_forwarding"
                | "default/force_forwarding"
        );
        (!special).then_some(path.as_str())
    }));
    order
}

#[cfg(target_os = "linux")]
fn rule_command(operation: &str, rule: &NatRule) -> std::process::Command {
    let mut command = std::process::Command::new("ip6tables");
    command
        .args(["-t", "nat", operation, "POSTROUTING", "!", "-o"])
        .arg(&rule.ifname)
        .arg("-s")
        .arg(&rule.prefix)
        .args(["-j", "MASQUERADE"]);
    command
}

#[cfg(target_os = "linux")]
fn command_error(action: &str, output: std::process::Output) -> anyhow::Error {
    anyhow!(
        "{action} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(target_os = "linux")]
fn linux_namespace_identity() -> Result<NamespaceIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata("/proc/thread-self/ns/net")
        .context("reading current network namespace identity")?;
    Ok(NamespaceIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(target_os = "linux")]
async fn current_namespace_identity(net_ns: NetNS) -> Result<NamespaceIdentity> {
    tokio::task::spawn_blocking(move || net_ns.run(linux_namespace_identity))
        .await
        .context("network namespace identity worker failed")?
}

#[cfg(target_os = "linux")]
async fn acquire_in_namespace(
    net_ns: NetNS,
    owner_id: u64,
    rule: NatRule,
) -> Result<NamespaceIdentity> {
    tokio::task::spawn_blocking(move || {
        net_ns.run(|| {
            let namespace = linux_namespace_identity()?;
            let mut manager = lease_manager()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            manager.acquire(&namespace, owner_id, &rule, &mut LinuxOps)?;
            Ok(namespace)
        })
    })
    .await
    .context("IPv6 forwarding acquisition worker failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeOps {
        forwarding: HashMap<NamespaceIdentity, ForwardingSnapshot>,
        rules: HashSet<RuleIdentity>,
        fail_enable_after_mutation: bool,
        fail_add: bool,
        fail_delete_count: usize,
        fail_restore_count: usize,
        restore_calls: usize,
        added_rules: usize,
        deleted_rules: usize,
    }

    impl PlatformOps for FakeOps {
        fn snapshot_forwarding(
            &mut self,
            namespace: &NamespaceIdentity,
        ) -> Result<ForwardingSnapshot> {
            Ok(self.forwarding.get(namespace).cloned().unwrap_or_default())
        }

        fn enable_forwarding(&mut self, namespace: &NamespaceIdentity) -> Result<()> {
            let snapshot = self.forwarding.entry(namespace.clone()).or_default();
            for (path, value) in &mut snapshot.values {
                if path.ends_with("/forwarding") {
                    *value = b"1\n".to_vec();
                }
            }
            if self.fail_enable_after_mutation {
                self.fail_enable_after_mutation = false;
                return Err(anyhow!("injected enable failure"));
            }
            Ok(())
        }

        fn restore_forwarding(
            &mut self,
            namespace: &NamespaceIdentity,
            snapshot: &ForwardingSnapshot,
        ) -> Result<()> {
            self.restore_calls += 1;
            if self.fail_restore_count > 0 {
                self.fail_restore_count -= 1;
                return Err(anyhow!("injected restore failure"));
            }
            self.forwarding.insert(namespace.clone(), snapshot.clone());
            Ok(())
        }

        fn rule_exists(&mut self, namespace: &NamespaceIdentity, rule: &NatRule) -> Result<bool> {
            Ok(self.rules.contains(&RuleIdentity {
                namespace: namespace.clone(),
                rule: rule.clone(),
            }))
        }

        fn add_rule(&mut self, namespace: &NamespaceIdentity, rule: &NatRule) -> Result<()> {
            if self.fail_add {
                return Err(anyhow!("injected rule-add failure"));
            }
            self.added_rules += 1;
            self.rules.insert(RuleIdentity {
                namespace: namespace.clone(),
                rule: rule.clone(),
            });
            Ok(())
        }

        fn delete_rule(&mut self, namespace: &NamespaceIdentity, rule: &NatRule) -> Result<()> {
            if self.fail_delete_count > 0 {
                self.fail_delete_count -= 1;
                return Err(anyhow!("injected rule-delete failure"));
            }
            self.deleted_rules += 1;
            self.rules.remove(&RuleIdentity {
                namespace: namespace.clone(),
                rule: rule.clone(),
            });
            Ok(())
        }
    }

    fn namespace(inode: u64) -> NamespaceIdentity {
        NamespaceIdentity { device: 1, inode }
    }

    fn rule() -> NatRule {
        NatRule {
            prefix: "fd00::/64".to_owned(),
            ifname: "tun0".to_owned(),
        }
    }

    fn snapshot(entries: &[(&str, &str)]) -> ForwardingSnapshot {
        ForwardingSnapshot {
            values: entries
                .iter()
                .map(|(path, value)| (path.to_string(), format!("{value}\n").into_bytes()))
                .collect(),
        }
    }

    #[test]
    fn restores_mixed_forwarding_values_after_last_owner() {
        let ns = namespace(10);
        let original = snapshot(&[
            ("all/forwarding", "0"),
            ("default/forwarding", "1"),
            ("eth0/forwarding", "0"),
            ("eth1/forwarding", "1"),
            ("eth1/force_forwarding", "1"),
        ]);
        let mut ops = FakeOps::default();
        ops.forwarding.insert(ns.clone(), original.clone());
        let mut manager = LeaseManager::default();

        manager.acquire(&ns, 1, &rule(), &mut ops).unwrap();
        assert_ne!(ops.forwarding.get(&ns), Some(&original));
        let mut active = ActiveLease {
            owner_id: 1,
            namespace: ns.clone(),
            rule: rule(),
            nat_held: true,
            forwarding_held: true,
        };
        manager.release(&mut active, &mut ops).unwrap();

        assert_eq!(ops.forwarding.get(&ns), Some(&original));
        assert!(!active.has_resources());
    }

    #[test]
    fn restores_aggregate_sysctls_before_interface_values() {
        let baseline = snapshot(&[
            ("eth0/forwarding", "0"),
            ("all/forwarding", "1"),
            ("default/forwarding", "0"),
            ("eth0/force_forwarding", "1"),
            ("all/force_forwarding", "0"),
        ]);

        assert_eq!(
            forwarding_restore_order(&baseline),
            [
                "all/forwarding",
                "default/forwarding",
                "all/force_forwarding",
                "eth0/force_forwarding",
                "eth0/forwarding",
            ]
        );
    }

    #[test]
    fn leaves_preexisting_nat_rule_untouched() {
        let ns = namespace(11);
        let rule = rule();
        let identity = RuleIdentity {
            namespace: ns.clone(),
            rule: rule.clone(),
        };
        let mut ops = FakeOps::default();
        ops.forwarding.insert(
            ns.clone(),
            snapshot(&[("all/forwarding", "0"), ("default/forwarding", "0")]),
        );
        ops.rules.insert(identity.clone());
        let mut manager = LeaseManager::default();

        manager.acquire(&ns, 1, &rule, &mut ops).unwrap();
        let mut active = ActiveLease {
            owner_id: 1,
            namespace: ns.clone(),
            rule,
            nat_held: true,
            forwarding_held: true,
        };
        manager.release(&mut active, &mut ops).unwrap();

        assert!(ops.rules.contains(&identity));
        assert_eq!(ops.deleted_rules, 0);
    }

    #[test]
    fn shared_owners_keep_forwarding_and_owned_rule_until_last_release() {
        let ns = namespace(12);
        let rule = rule();
        let original = snapshot(&[("all/forwarding", "0"), ("default/forwarding", "0")]);
        let identity = RuleIdentity {
            namespace: ns.clone(),
            rule: rule.clone(),
        };
        let mut ops = FakeOps::default();
        ops.forwarding.insert(ns.clone(), original.clone());
        let mut manager = LeaseManager::default();

        manager.acquire(&ns, 1, &rule, &mut ops).unwrap();
        manager.acquire(&ns, 2, &rule, &mut ops).unwrap();
        assert_eq!(ops.added_rules, 1);

        let mut first = ActiveLease {
            owner_id: 1,
            namespace: ns.clone(),
            rule: rule.clone(),
            nat_held: true,
            forwarding_held: true,
        };
        manager.release(&mut first, &mut ops).unwrap();
        assert!(ops.rules.contains(&identity));
        assert_ne!(ops.forwarding.get(&ns), Some(&original));

        let mut last = ActiveLease {
            owner_id: 2,
            namespace: ns.clone(),
            rule,
            nat_held: true,
            forwarding_held: true,
        };
        manager.release(&mut last, &mut ops).unwrap();
        assert!(!ops.rules.contains(&identity));
        assert_eq!(ops.forwarding.get(&ns), Some(&original));
        assert_eq!(ops.deleted_rules, 1);
    }

    #[test]
    fn failed_install_rolls_back_and_failed_rollback_is_retryable() {
        let ns = namespace(13);
        let original = snapshot(&[("all/forwarding", "0"), ("default/forwarding", "1")]);
        let mut ops = FakeOps::default();
        ops.forwarding.insert(ns.clone(), original.clone());
        ops.fail_add = true;
        ops.fail_restore_count = 1;
        let mut manager = LeaseManager::default();

        assert!(manager.acquire(&ns, 1, &rule(), &mut ops).is_err());
        assert!(manager.forwarding.get(&ns).unwrap().owners.is_empty());
        assert_ne!(ops.forwarding.get(&ns), Some(&original));

        ops.fail_add = false;
        manager.acquire(&ns, 2, &rule(), &mut ops).unwrap();
        assert_eq!(ops.restore_calls, 2);
        assert_eq!(
            ops.forwarding.get(&ns).unwrap().values["all/forwarding"].as_slice(),
            b"1\n"
        );

        let mut active = ActiveLease {
            owner_id: 2,
            namespace: ns.clone(),
            rule: rule(),
            nat_held: true,
            forwarding_held: true,
        };
        manager.release(&mut active, &mut ops).unwrap();
        assert_eq!(ops.forwarding.get(&ns), Some(&original));
    }

    #[test]
    fn failed_release_keeps_the_remaining_lease_state_for_retry() {
        let ns = namespace(16);
        let original = snapshot(&[("all/forwarding", "0"), ("default/forwarding", "0")]);
        let identity = RuleIdentity {
            namespace: ns.clone(),
            rule: rule(),
        };
        let mut ops = FakeOps::default();
        ops.forwarding.insert(ns.clone(), original.clone());
        let mut manager = LeaseManager::default();
        manager.acquire(&ns, 1, &rule(), &mut ops).unwrap();
        let mut active = ActiveLease {
            owner_id: 1,
            namespace: ns.clone(),
            rule: rule(),
            nat_held: true,
            forwarding_held: true,
        };
        ops.fail_restore_count = 1;

        assert!(manager.release(&mut active, &mut ops).is_err());
        assert!(!active.nat_held);
        assert!(active.forwarding_held);
        assert!(!ops.rules.contains(&identity));

        manager.release(&mut active, &mut ops).unwrap();
        assert!(!active.has_resources());
        assert_eq!(ops.forwarding.get(&ns), Some(&original));
    }

    #[test]
    fn dropped_failed_lease_leaves_pending_cleanup_for_next_owner() {
        let ns = namespace(17);
        let original = snapshot(&[("all/forwarding", "0"), ("default/forwarding", "0")]);
        let mut ops = FakeOps::default();
        ops.forwarding.insert(ns.clone(), original.clone());
        let mut manager = LeaseManager::default();
        manager.acquire(&ns, 1, &rule(), &mut ops).unwrap();
        let mut abandoned = ActiveLease {
            owner_id: 1,
            namespace: ns.clone(),
            rule: rule(),
            nat_held: true,
            forwarding_held: true,
        };
        ops.fail_delete_count = 1;

        assert!(manager.release(&mut abandoned, &mut ops).is_err());
        assert!(abandoned.nat_held);
        assert!(abandoned.forwarding_held);
        manager.abandon_owner(abandoned.owner_id);
        assert!(
            manager
                .nat_rules
                .values()
                .all(|state| state.owners.is_empty())
        );
        assert!(manager.forwarding.get(&ns).unwrap().owners.is_empty());

        manager.acquire(&ns, 2, &rule(), &mut ops).unwrap();
        assert_eq!(ops.deleted_rules, 1);
        let mut next = ActiveLease {
            owner_id: 2,
            namespace: ns.clone(),
            rule: rule(),
            nat_held: true,
            forwarding_held: true,
        };
        manager.release(&mut next, &mut ops).unwrap();
        assert_eq!(ops.forwarding.get(&ns), Some(&original));
    }

    #[test]
    fn namespace_forwarding_owners_are_independent() {
        let first_ns = namespace(14);
        let second_ns = namespace(15);
        let first_original = snapshot(&[("all/forwarding", "0"), ("default/forwarding", "0")]);
        let second_original = snapshot(&[("all/forwarding", "1"), ("default/forwarding", "0")]);
        let mut ops = FakeOps::default();
        ops.forwarding
            .insert(first_ns.clone(), first_original.clone());
        ops.forwarding
            .insert(second_ns.clone(), second_original.clone());
        let mut manager = LeaseManager::default();

        manager.acquire(&first_ns, 1, &rule(), &mut ops).unwrap();
        manager.acquire(&second_ns, 2, &rule(), &mut ops).unwrap();
        let mut first = ActiveLease {
            owner_id: 1,
            namespace: first_ns.clone(),
            rule: rule(),
            nat_held: true,
            forwarding_held: true,
        };
        manager.release(&mut first, &mut ops).unwrap();

        assert_eq!(ops.forwarding.get(&first_ns), Some(&first_original));
        assert_ne!(ops.forwarding.get(&second_ns), Some(&second_original));
        assert!(ops.rules.contains(&RuleIdentity {
            namespace: second_ns.clone(),
            rule: rule(),
        }));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod namespace_integration_tests {
    use super::*;
    use anyhow::{Context, Result, anyhow};
    use std::process::Command;

    static NEXT_NAMESPACE_ID: AtomicU64 = AtomicU64::new(1);

    struct TestNamespace {
        name: String,
        created: bool,
    }

    impl TestNamespace {
        fn new() -> Result<Self> {
            let id = NEXT_NAMESPACE_ID.fetch_add(1, Ordering::Relaxed);
            let namespace = Self {
                name: format!("et-vpn-{}-{id}", std::process::id()),
                created: true,
            };
            let output = Command::new("ip")
                .args(["netns", "add", &namespace.name])
                .output()
                .context("creating an isolated test network namespace")?;
            if !output.status.success() {
                return Err(anyhow!(
                    "ip netns add failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok(namespace)
        }

        fn net_ns(&self) -> NetNS {
            NetNS::new(Some(self.name.clone()))
        }

        fn cleanup(&mut self) -> Result<()> {
            if !self.created {
                return Ok(());
            }
            let output = Command::new("ip")
                .args(["netns", "del", &self.name])
                .output()
                .context("deleting an isolated test network namespace")?;
            if !output.status.success() {
                return Err(anyhow!(
                    "ip netns del failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            self.created = false;
            Ok(())
        }
    }

    impl Drop for TestNamespace {
        fn drop(&mut self) {
            if self.created {
                let _ = Command::new("ip")
                    .args(["netns", "del", &self.name])
                    .output();
                self.created = false;
            }
        }
    }

    async fn in_namespace<T, F>(net_ns: NetNS, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        tokio::task::spawn_blocking(move || net_ns.run(operation))
            .await
            .context("isolated namespace test worker failed")?
    }

    async fn set_baseline(
        namespace: &TestNamespace,
        values: [u8; 3],
    ) -> Result<ForwardingSnapshot> {
        let net_ns = namespace.net_ns();
        in_namespace(net_ns, move || {
            let root = std::path::Path::new("/proc/sys/net/ipv6/conf");
            for (setting, value) in [
                ("all/forwarding", values[0]),
                ("default/forwarding", values[1]),
                ("lo/forwarding", values[2]),
            ] {
                let value = if value == 0 { b"0" } else { b"1" };
                std::fs::write(root.join(setting), value)
                    .with_context(|| format!("setting isolated test sysctl {setting}"))?;
            }
            let namespace = linux_namespace_identity()?;
            LinuxOps.snapshot_forwarding(&namespace)
        })
        .await
    }

    async fn snapshot(namespace: &TestNamespace) -> Result<ForwardingSnapshot> {
        let net_ns = namespace.net_ns();
        in_namespace(net_ns, || {
            let namespace = linux_namespace_identity()?;
            LinuxOps.snapshot_forwarding(&namespace)
        })
        .await
    }

    async fn set_rule(namespace: &TestNamespace, rule: NatRule, add: bool) -> Result<()> {
        let net_ns = namespace.net_ns();
        in_namespace(net_ns, move || {
            let namespace = linux_namespace_identity()?;
            let mut ops = LinuxOps;
            if add {
                ops.add_rule(&namespace, &rule)
            } else {
                ops.delete_rule(&namespace, &rule)
            }
        })
        .await
    }

    async fn has_rule(namespace: &TestNamespace, rule: NatRule) -> Result<bool> {
        let net_ns = namespace.net_ns();
        in_namespace(net_ns, move || {
            let namespace = linux_namespace_identity()?;
            let mut ops = LinuxOps;
            ops.rule_exists(&namespace, &rule)
        })
        .await
    }

    fn rule(prefix: &str) -> NatRule {
        NatRule {
            prefix: prefix.to_owned(),
            ifname: "et-tun0".to_owned(),
        }
    }

    async fn assert_roundtrip(
        namespace: &TestNamespace,
        baseline_values: [u8; 3],
        prefix: &str,
    ) -> Result<ForwardingSnapshot> {
        let baseline = set_baseline(namespace, baseline_values).await?;
        let rule = rule(prefix);
        let mut lease = VpnExitForwardingLease::new(namespace.net_ns());

        lease
            .reconcile("et-tun0", Some(rule.prefix.clone()), true)
            .await?;
        assert_eq!(lease.prefix(), Some(rule.prefix.as_str()));
        assert_eq!(lease.ifname(), Some(rule.ifname.as_str()));
        assert!(has_rule(namespace, rule.clone()).await?);
        lease.release().await?;

        assert!(!has_rule(namespace, rule).await?);
        assert_eq!(snapshot(namespace).await?, baseline);
        Ok(baseline)
    }

    #[tokio::test]
    async fn linux_lease_restores_sysctls_and_only_owned_nat_in_fresh_namespaces() -> Result<()> {
        if std::env::var("EASYTIER_RUN_VPN_EXIT_FORWARDING_NETNS_TESTS").as_deref() != Ok("1") {
            return Ok(());
        }

        let mut baseline_zero_ns = TestNamespace::new()?;
        let mut baseline_one_ns = TestNamespace::new()?;
        let mut mixed_ns = TestNamespace::new()?;
        let mut preexisting_ns = TestNamespace::new()?;

        let baseline_zero =
            assert_roundtrip(&baseline_zero_ns, [0, 0, 0], "fd70:0:0:1::/64").await?;
        let baseline_one = assert_roundtrip(&baseline_one_ns, [1, 1, 1], "fd70:0:0:2::/64").await?;
        let baseline_mixed = set_baseline(&mixed_ns, [0, 1, 1]).await?;

        let first_rule = rule("fd70:0:0:3::/64");
        let replacement_rule = rule("fd70:0:0:4::/64");
        let mut first = VpnExitForwardingLease::new(mixed_ns.net_ns());
        let mut second = VpnExitForwardingLease::new(mixed_ns.net_ns());
        first
            .reconcile("et-tun0", Some(first_rule.prefix.clone()), true)
            .await?;
        second
            .reconcile("et-tun0", Some(first_rule.prefix.clone()), true)
            .await?;
        first
            .reconcile("et-tun0", Some(replacement_rule.prefix.clone()), true)
            .await?;
        assert!(has_rule(&mixed_ns, first_rule.clone()).await?);
        assert!(has_rule(&mixed_ns, replacement_rule.clone()).await?);

        first.release().await?;
        assert!(has_rule(&mixed_ns, first_rule.clone()).await?);
        assert!(!has_rule(&mixed_ns, replacement_rule).await?);
        let active_snapshot = snapshot(&mixed_ns).await?;
        assert_eq!(active_snapshot.values["all/forwarding"].as_slice(), b"1\n");
        second.release().await?;
        assert!(!has_rule(&mixed_ns, first_rule).await?);
        assert_eq!(snapshot(&mixed_ns).await?, baseline_mixed);

        let preexisting_rule = rule("fd70:0:0:5::/64");
        let baseline_preexisting = set_baseline(&preexisting_ns, [1, 0, 1]).await?;
        set_rule(&preexisting_ns, preexisting_rule.clone(), true).await?;
        let mut preexisting_lease = VpnExitForwardingLease::new(preexisting_ns.net_ns());
        preexisting_lease
            .reconcile("et-tun0", Some(preexisting_rule.prefix.clone()), true)
            .await?;
        preexisting_lease.release().await?;
        assert!(has_rule(&preexisting_ns, preexisting_rule.clone()).await?);
        assert_eq!(snapshot(&preexisting_ns).await?, baseline_preexisting);
        set_rule(&preexisting_ns, preexisting_rule, false).await?;

        let independent_rule = rule("fd70:0:0:6::/64");
        let baseline_zero_identity = current_namespace_identity(baseline_zero_ns.net_ns()).await?;
        let baseline_one_identity = current_namespace_identity(baseline_one_ns.net_ns()).await?;
        assert_ne!(
            baseline_zero_identity, baseline_one_identity,
            "the independent lease cases must use distinct network namespaces"
        );
        let mut first_namespace_lease = VpnExitForwardingLease::new(baseline_zero_ns.net_ns());
        let mut second_namespace_lease = VpnExitForwardingLease::new(baseline_one_ns.net_ns());
        first_namespace_lease
            .reconcile("et-tun0", Some(independent_rule.prefix.clone()), true)
            .await?;
        second_namespace_lease
            .reconcile("et-tun0", Some(independent_rule.prefix.clone()), true)
            .await?;
        first_namespace_lease.release().await?;
        let lingering_rule = has_rule(&baseline_zero_ns, independent_rule.clone()).await?;
        let tracked_owners = lease_manager()
            .lock()
            .unwrap()
            .nat_rules
            .get(&RuleIdentity {
                namespace: baseline_zero_identity.clone(),
                rule: independent_rule.clone(),
            })
            .map(|state| state.owners.len())
            .unwrap_or_default();
        assert!(
            !lingering_rule,
            "independent rule remains in {baseline_zero_identity:?} after release; manager tracks {tracked_owners} owners"
        );
        assert_eq!(snapshot(&baseline_zero_ns).await?, baseline_zero);
        assert!(has_rule(&baseline_one_ns, independent_rule.clone()).await?);
        assert_eq!(
            snapshot(&baseline_one_ns).await?.values["all/forwarding"].as_slice(),
            b"1\n"
        );
        second_namespace_lease.release().await?;
        assert_eq!(snapshot(&baseline_one_ns).await?, baseline_one);
        assert!(!has_rule(&baseline_one_ns, independent_rule).await?);

        baseline_zero_ns.cleanup()?;
        baseline_one_ns.cleanup()?;
        mixed_ns.cleanup()?;
        preexisting_ns.cleanup()?;
        Ok(())
    }
}
