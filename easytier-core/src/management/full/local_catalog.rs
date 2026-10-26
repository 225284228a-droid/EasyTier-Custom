//! The node owns this read-only projection of its persisted configuration.
//! Every console observes the same immutable generation; consoles never
//! publish the projection back to the node as desired state.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use easytier_proto::api::manage::{ObserveConfigsResponse, PersistedConfigEntry};
use sha2::{Digest as _, Sha256};

use super::{
    ConfigFilePermission, ConfigFileStorage, InstanceMutationHooks, InstanceStateStore,
    ProcessManagement, config_source_to_rpc,
};
use crate::{
    config::{
        api::network_config_from_toml,
        toml::{ConfigLoader as _, TomlConfig},
    },
    instance::{
        CoreInstance, CoreInstanceHost,
        manager::{InstanceFactory, InstanceManager},
    },
};

pub const CATALOG_SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Shared across RPC registries and reverse connections of one node.
pub struct LocalConfigCatalog {
    snapshot: Mutex<ObserveConfigsResponse>,
    observer_started: AtomicBool,
    refresh_requested: tokio::sync::Notify,
    observer_cancel: tokio_util::sync::CancellationToken,
    state_store: OnceLock<Arc<InstanceStateStore>>,
    pub(super) reconciled_files: Mutex<BTreeMap<PathBuf, super::local_reconcile::ReconciledFile>>,
    acknowledged_files: Mutex<BTreeMap<PathBuf, String>>,
}

pub(crate) struct MutationRefreshGuard(Arc<LocalConfigCatalog>);

impl Drop for MutationRefreshGuard {
    fn drop(&mut self) {
        self.0.request_refresh();
    }
}

impl Default for LocalConfigCatalog {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(ObserveConfigsResponse {
                catalog_epoch: uuid::Uuid::new_v4().to_string(),
                catalog_generation: 0,
                entries: Vec::new(),
            }),
            observer_started: AtomicBool::new(false),
            refresh_requested: tokio::sync::Notify::new(),
            observer_cancel: tokio_util::sync::CancellationToken::new(),
            state_store: OnceLock::new(),
            reconciled_files: Mutex::new(BTreeMap::new()),
            acknowledged_files: Mutex::new(BTreeMap::new()),
        }
    }
}

impl LocalConfigCatalog {
    pub(super) fn shared_state_store(
        &self,
        proposed: Arc<InstanceStateStore>,
    ) -> Arc<InstanceStateStore> {
        self.state_store.get_or_init(|| proposed).clone()
    }

    pub(crate) fn state_store(&self) -> Option<Arc<InstanceStateStore>> {
        self.state_store.get().cloned()
    }

    pub fn supports_revision(&self) -> bool {
        self.observer_started.load(Ordering::Acquire)
    }

    pub fn request_refresh(&self) {
        self.refresh_requested.notify_one();
    }

    /// Retire this manager's file owner before draining its instances. A mode
    /// switch must not interpret that drain as enabled networks needing restart.
    pub fn retire(&self) {
        self.observer_cancel.cancel();
    }

    pub fn is_retired(&self) -> bool {
        self.observer_cancel.is_cancelled()
    }

    /// The writer of these contents already applied them (or the network is
    /// disabled). Its own filesystem notification must not restart it again.
    pub fn acknowledge_file(&self, path: &Path, contents: &[u8]) {
        self.acknowledged_files
            .lock()
            .unwrap()
            .insert(path.to_owned(), raw_hash(contents));
    }

    pub(super) fn acknowledged_file(&self, path: &Path, hash: &str) -> bool {
        self.acknowledged_files
            .lock()
            .unwrap()
            .remove(path)
            .is_some_and(|known| known == hash)
    }

    pub(super) fn forget_file(&self, path: &Path) {
        self.acknowledged_files.lock().unwrap().remove(path);
    }

    pub(super) fn forget_missing_files(&self, paths: &HashSet<PathBuf>) {
        self.acknowledged_files
            .lock()
            .unwrap()
            .retain(|path, _| paths.contains(path));
    }

    pub(crate) fn refresh_after_mutation(self: &Arc<Self>) -> MutationRefreshGuard {
        MutationRefreshGuard(self.clone())
    }

    pub fn version(&self) -> (String, u64) {
        let snapshot = self.snapshot.lock().unwrap();
        (snapshot.catalog_epoch.clone(), snapshot.catalog_generation)
    }

    pub fn snapshot(&self) -> ObserveConfigsResponse {
        self.snapshot.lock().unwrap().clone()
    }

    /// Called only under the process mutation coordinator. The first scan
    /// establishes a known generation even for an empty directory.
    fn publish(&self, entries: Vec<PersistedConfigEntry>) -> (ObserveConfigsResponse, bool) {
        let mut snapshot = self.snapshot.lock().unwrap();
        let changed = snapshot.catalog_generation == 0 || snapshot.entries != entries;
        if changed {
            snapshot.catalog_generation = snapshot.catalog_generation.saturating_add(1);
            snapshot.entries = entries;
        }
        (snapshot.clone(), changed)
    }
}

pub(crate) fn raw_hash(contents: &[u8]) -> String {
    Sha256::digest(contents)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
fn semantic_hash(contents: &str) -> Option<String> {
    let parsed = TomlConfig::new_from_str(contents).ok()?;
    if implicit_secure_identity(contents.as_bytes()) {
        omit_generated_identity(&parsed);
    }
    config_semantic_hash(&parsed)
}

fn implicit_secure_identity(raw: &[u8]) -> bool {
    std::str::from_utf8(raw)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(text).ok())
        .and_then(|table| {
            table
                .get("secure_mode")
                .and_then(toml::Value::as_table)
                .cloned()
        })
        .is_some_and(|secure| {
            !secure.contains_key("local_private_key") && !secure.contains_key("localPrivateKey")
        })
}

fn omit_generated_identity(config: &TomlConfig) {
    if let Some(mut secure) = config.get_secure_mode() {
        secure.local_private_key = None;
        secure.local_public_key = None;
        config.set_secure_mode(Some(secure));
    }
}

/// Parsing an enabled secure config supplies a random local key when the file
/// omits it. An external edit must retain the active node's implicit identity.
/// Explicit keys (including environment references) always remain authoritative.
pub(super) fn reuse_implicit_identity(
    raw: &[u8],
    config: &TomlConfig,
    active: Option<&TomlConfig>,
) {
    if !implicit_secure_identity(raw) {
        return;
    }
    if let Some(mut secure) = config.get_secure_mode().filter(|secure| secure.enabled)
        && let Some(identity) = active
            .and_then(|active| active.get_secure_mode())
            .filter(|secure| secure.enabled)
    {
        secure.local_private_key = identity.local_private_key;
        secure.local_public_key = identity.local_public_key;
        config.set_secure_mode(Some(secure));
    }
}

fn config_semantic_hash(config: &TomlConfig) -> Option<String> {
    // A dump preserves user-supplied flag presence. For runtime comparison,
    // resolve defaults only in this document, leaving the configuration and
    // its management API projection untouched.
    let mut normalized = toml::from_str::<toml::Table>(&config.dump()).ok()?;
    let mut resolved_flags = config.get_flags();
    // Compare the disguise policy, not its spelling.
    resolved_flags.set_p2p_disguise(resolved_flags.p2p_disguise());
    normalized.insert(
        "flags".to_owned(),
        toml::Value::try_from(resolved_flags).ok()?,
    );
    Some(raw_hash(toml::to_string(&normalized).ok()?.as_bytes()))
}

pub(super) fn configs_equivalent(left: &TomlConfig, right: &TomlConfig) -> bool {
    config_semantic_hash(left)
        .zip(config_semantic_hash(right))
        .is_some_and(|(left, right)| left == right)
}

/// The revision identifies the file a client edited: its name, contents
/// and permissions. Runtime state never invalidates an open editor, and the
/// same file keeps its revision across node restarts and consoles.
fn entry_revision(entry: &PersistedConfigEntry) -> String {
    raw_hash(
        format!(
            "{}\0{}\0{}\0{}",
            entry.entry_key, entry.persisted_raw_hash, entry.config_permission, entry.status
        )
        .as_bytes(),
    )
}

fn filename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Observe without changing running instances. Invalid and protected files
/// have metadata-only entries; parser errors never include source text.
pub(crate) async fn refresh_configs<F, H>(
    manager: &Arc<InstanceManager<F>>,
    storage: &dyn ConfigFileStorage,
    state: &InstanceStateStore,
) -> anyhow::Result<ObserveConfigsResponse>
where
    F: InstanceFactory<Instance = CoreInstance<H>>,
    H: CoreInstanceHost,
{
    let Some(directory) = manager.config_dir() else {
        anyhow::bail!("persisted config directory is unavailable");
    };
    if !storage.supports_catalog() {
        anyhow::bail!("persisted config observation is unavailable");
    }
    let paths = storage.list_configs(directory).await?;
    let mut entries = BTreeMap::<String, PersistedConfigEntry>::new();
    let mut owners = BTreeMap::<uuid::Uuid, Vec<String>>::new();
    let mut observed_paths = HashSet::<PathBuf>::new();
    for path in paths {
        if path.parent() != Some(directory.as_path())
            || path.extension() != Some(std::ffi::OsStr::new("toml"))
        {
            continue;
        }
        observed_paths.insert(path.clone());
        let key = filename(&path);
        let mut entry = PersistedConfigEntry {
            entry_key: key.clone(),
            status: "invalid".into(),
            ..Default::default()
        };
        let mut active_config = None;
        let raw = match storage.read(&path).await {
            Ok(Some(raw)) => raw,
            Ok(None) => continue,
            Err(_) => {
                entry.status = "protected".into();
                entry.config_permission =
                    u32::from(ConfigFilePermission::READ_ONLY | ConfigFilePermission::NO_DELETE);
                if let Some(id) = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse::<uuid::Uuid>().ok())
                {
                    entry.inst_id = Some(id.into());
                    entry.running = manager.instance(id).is_some();
                    entry.enabled = state.is_enabled(&id);
                    owners.entry(id).or_default().push(key.clone());
                }
                entry.revision = entry_revision(&entry);
                entries.insert(key, entry);
                continue;
            }
        };
        entry.persisted_raw_hash = raw_hash(&raw);
        // UUID identity is inspected in the unexpanded original document.
        // Never invoke ConfigLoader.get_id() before checking its presence:
        // that accessor generates a random UUID for missing identities.
        let text = std::str::from_utf8(&raw).ok();
        let table = text.and_then(|text| toml::from_str::<toml::Table>(text).ok());
        let identity = table
            .as_ref()
            .and_then(|table| table.get("instance_id"))
            .and_then(toml::Value::as_str)
            .and_then(|id| id.parse::<uuid::Uuid>().ok());
        let identity = identity.or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse().ok())
        });
        if let Some(id) = identity {
            entry.inst_id = Some(id.into());
            entry.running = manager.instance(id).is_some();
            entry.enabled = state.is_enabled(&id);
            if let Some(config) = manager.config(id) {
                entry.active_raw_hash = Some(raw_hash(config.dump().as_bytes()));
                active_config = Some(config);
            }
            owners.entry(id).or_default().push(key.clone());
        }
        let mut control = storage
            .inspect_raw_config(&path, &raw, directory)
            .await
            .unwrap_or_else(|_| {
                super::ConfigFileControl::new(
                    Some(path.clone()),
                    ConfigFilePermission::from(
                        ConfigFilePermission::READ_ONLY | ConfigFilePermission::NO_DELETE,
                    ),
                )
            });
        if let Some(id) = identity {
            // Running protected configuration can contain expanded secrets.
            // Stopped files instead receive fresh permissions from disk.
            for additional in manager.config_control(id).into_iter() {
                control.permission = ConfigFilePermission::from(
                    u8::from(control.permission) | u8::from(additional.permission),
                );
                if additional.path.as_ref() != Some(&path) {
                    entry.status = "identity_conflict".into();
                }
            }
        }
        entry.config_permission = u8::from(control.permission).into();
        if control.is_read_only() {
            entry.status = "protected".into();
        } else if entry.status != "identity_conflict"
            && let (Some(id), Some(text), Some(table)) = (identity, text, table.as_ref())
            && table
                .get("instance_id")
                .and_then(toml::Value::as_str)
                .and_then(|id| id.parse::<uuid::Uuid>().ok())
                == Some(id)
            && let Ok(config) = TomlConfig::new_from_str(text)
        {
            reuse_implicit_identity(&raw, &config, active_config.as_ref());
            if implicit_secure_identity(&raw)
                && active_config
                    .as_ref()
                    .and_then(|active| active.get_secure_mode())
                    .is_none_or(|secure| !secure.enabled)
            {
                // There is no active identity to report for a stopped file.
                // Project its durable intent, avoiding a new generated key on
                // every read and a spuriously increasing catalog generation.
                omit_generated_identity(&config);
            }
            entry.status = "ready".into();
            entry.network_name = config.get_network_identity().network_name;
            entry.source = config_source_to_rpc(config.get_network_config_source());
            entry.config = Some(network_config_from_toml(&config));
            entry.persisted_toml = Some(text.to_owned());
            if manager
                .register_managed_config(id, control.clone())
                .is_err()
            {
                entry.status = "identity_conflict".into();
                entry.config = None;
                entry.persisted_toml = None;
            }
        }
        entry.revision = entry_revision(&entry);
        entries.insert(key, entry);
    }
    for keys in owners.values().filter(|keys| keys.len() > 1) {
        for key in keys {
            let entry = entries.get_mut(key).unwrap();
            entry.status = "identity_conflict".into();
            entry.config = None;
            entry.persisted_toml = None;
            entry.config_permission |=
                u32::from(ConfigFilePermission::READ_ONLY | ConfigFilePermission::NO_DELETE);
            entry.revision = entry_revision(entry);
        }
    }
    // Keep a metadata-only record for a running instance whose file was
    // removed externally. A disk edit must never stop that instance.
    for id in manager.instance_ids() {
        if owners.contains_key(&id) {
            continue;
        }
        let control = manager.config_control(id);
        let Some(control) = control else {
            continue;
        };
        let in_directory =
            control.path.as_deref().and_then(Path::parent) == Some(directory.as_path());
        let mut entry = PersistedConfigEntry {
            entry_key: if in_directory {
                filename(control.path.as_deref().unwrap())
            } else {
                format!("runtime:{id}")
            },
            inst_id: Some(id.into()),
            running: true,
            enabled: state.is_enabled(&id),
            config_permission: u8::from(control.permission).into(),
            active_raw_hash: manager
                .config(id)
                .map(|config| raw_hash(config.dump().as_bytes())),
            status: if in_directory {
                "missing".into()
            } else {
                "protected".into()
            },
            ..Default::default()
        };
        if !in_directory {
            entry.config_permission |=
                u32::from(ConfigFilePermission::READ_ONLY | ConfigFilePermission::NO_DELETE);
        }
        entry.revision = entry_revision(&entry);
        entries.insert(entry.entry_key.clone(), entry);
    }
    // Catalog membership is independent of runtime membership. Removal of a
    // stopped file only removes its projection, never schedules a restart.
    for id in manager.managed_config_ids() {
        if let Some(control) = manager.managed_config_control(id)
            && !control
                .path
                .as_ref()
                .is_some_and(|path| observed_paths.contains(path))
            && manager.instance(id).is_none()
        {
            manager.unregister_managed_config(id);
        }
    }
    let (snapshot, changed) = manager
        .local_config_catalog()
        .publish(entries.into_values().collect());
    if changed {
        manager.notify_local_config_change();
    }
    Ok(snapshot)
}

pub(super) fn start_observer<F, H>(
    manager: &Arc<InstanceManager<F>>,
    hooks: Arc<dyn InstanceMutationHooks>,
    storage: Arc<dyn ConfigFileStorage>,
    state: Arc<InstanceStateStore>,
) where
    F: InstanceFactory<Instance = CoreInstance<H>, CreateContext = ()>,
    F::Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
    H: CoreInstanceHost,
{
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if manager.config_dir().is_none()
        || !storage.supports_catalog()
        || manager
            .local_config_catalog()
            .observer_started
            .swap(true, Ordering::AcqRel)
    {
        return;
    }
    let catalog = manager.local_config_catalog().clone();
    let live = storage.supports_live_reconcile();
    let changed_catalog = Arc::downgrade(&catalog);
    let watcher = if live {
        storage.watch_configs(manager.config_dir().unwrap(), Arc::new(move || {
            if let Some(catalog) = changed_catalog.upgrade() {
                catalog.request_refresh();
            }
        })).unwrap_or_else(|error| {
            tracing::warn!(%error, "config directory watcher unavailable; using periodic reconciliation");
            None
        })
    } else {
        None
    };
    let scan_interval = if live && watcher.is_none() {
        Duration::from_secs(1)
    } else {
        CATALOG_SCAN_INTERVAL
    };
    let manager = Arc::downgrade(manager);
    runtime.spawn(async move {
        let _watcher = watcher;
        let mut timer = tokio::time::interval(scan_interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = catalog.observer_cancel.cancelled() => return,
                _ = timer.tick() => {},
                _ = catalog.refresh_requested.notified() => {},
            }
            if live {
                // Editors commonly truncate then write or replace via rename.
                // Coalesce their event bursts before interpreting deletion.
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let Some(manager) = manager.upgrade() else {
                return;
            };
            let lock = manager.mutation_lock();
            let _operation = tokio::select! {
                _ = catalog.observer_cancel.cancelled() => return,
                operation = lock.lock() => operation,
            };
            // Shutdown may have acquired the coordinator while this scan was
            // debouncing or waiting for a preceding RPC transaction.
            if catalog.observer_cancel.is_cancelled() {
                return;
            }
            if live {
                let management = ProcessManagement {
                    instances: manager.clone(),
                    hooks: hooks.clone(),
                    storage: storage.clone(),
                    mutation_lock: lock.clone(),
                    state_store: state.clone(),
                };
                if let Err(error) = management.reconcile_local_files_locked().await {
                    tracing::warn!(%error, "local config reconciliation failed; retaining running instances");
                }
            }
            if refresh_configs(&manager, storage.as_ref(), &state)
                .await
                .is_err()
            {
                tracing::warn!(
                    "local config observation failed; keeping the last complete snapshot"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_identity_never_replaces_explicit_keys_or_environment_references() {
        let omitted = "[secure_mode]\nenabled=true\n";
        assert!(implicit_secure_identity(omitted.as_bytes()));
        for field in ["local_private_key", "localPrivateKey"] {
            for value in ["", " ", "${PRIVATE_KEY}", "not-base64"] {
                let explicit = format!("{omitted}{field}='{value}'\n");
                assert!(!implicit_secure_identity(explicit.as_bytes()));
            }
            for value in ["", " "] {
                let invalid = format!("{omitted}{field}='{value}'\n");
                assert!(
                    TomlConfig::new_from_str(&invalid).is_err(),
                    "blank explicit keys are rejected by the normalizer, not generated"
                );
            }
        }
    }

    #[test]
    fn empty_catalog_establishes_a_generation_and_does_not_repeat_it() {
        let catalog = LocalConfigCatalog::default();
        let (first, changed) = catalog.publish(Vec::new());
        assert!(changed);
        assert_eq!(first.catalog_generation, 1);
        let (second, changed) = catalog.publish(Vec::new());
        assert!(!changed);
        assert_eq!(first, second);
    }

    #[test]
    fn revision_fences_file_contents_and_permissions_but_not_runtime_state() {
        let entry = PersistedConfigEntry {
            entry_key: "one.toml".into(),
            persisted_raw_hash: raw_hash(b"config"),
            status: "ready".into(),
            ..Default::default()
        };
        let base = entry_revision(&entry);
        let mut changed = entry.clone();
        changed.config_permission = 1;
        assert_ne!(base, entry_revision(&changed));
        let mut changed = entry.clone();
        changed.entry_key = "two.toml".into();
        assert_ne!(base, entry_revision(&changed));
        let mut runtime = entry.clone();
        runtime.enabled = true;
        runtime.running = true;
        runtime.active_raw_hash = Some("active".into());
        assert_eq!(base, entry_revision(&runtime));
    }

    #[test]
    fn semantic_hash_ignores_comments_and_explicit_default_formatting() {
        let one =
            "instance_id='11111111-1111-1111-1111-111111111111'\n[flags]\ndisable_p2p=false\n";
        let two = "# comment\ninstance_id = '11111111-1111-1111-1111-111111111111'\n";
        assert_ne!(raw_hash(one.as_bytes()), raw_hash(two.as_bytes()));
        assert_eq!(semantic_hash(one), semantic_hash(two));

        // Equivalent runtime behavior still represents a different on-disk
        // revision, so an edit cannot bypass the raw-content fence.
        let mut entry = PersistedConfigEntry {
            entry_key: "one.toml".into(),
            persisted_raw_hash: raw_hash(one.as_bytes()),
            ..Default::default()
        };
        let revision = entry_revision(&entry);
        entry.persisted_raw_hash = raw_hash(two.as_bytes());
        assert_ne!(revision, entry_revision(&entry));
    }

    #[test]
    fn semantic_hash_resolves_flag_defaults_and_distinguishes_actual_changes() {
        let base = "instance_id='11111111-1111-1111-1111-111111111111'\n";
        let hash = semantic_hash(base).unwrap();
        for defaults in [
            "default_protocol = 'udp'",
            "default_protocol = ''",
            "enable_encryption = true",
            "disable_p2p = false",
            "close_redundant_conns_when_disguised = true",
            "prefer_wss_http3_for_p2p = true",
            "p2p_disguise_mode = 'prefer'",
            "p2p_disguise_mode = ' Prefer '",
            "mtu = 1380",
            "instance_recv_bps_limit = '18446744073709551615'",
            "dev_name = ''",
        ] {
            assert_eq!(
                hash,
                semantic_hash(&format!("{base}[flags]\n{defaults}")).unwrap(),
                "{defaults} should retain the default runtime behavior"
            );
        }
        for changed in [
            "default_protocol = 'tcp'",
            "enable_encryption = false",
            "close_redundant_conns_when_disguised = false",
            "prefer_wss_http3_for_p2p = false",
            "p2p_disguise_mode = 'off'",
            "mtu = 0",
            "instance_recv_bps_limit = 0",
            "socket_mark = 0",
        ] {
            assert_ne!(
                hash,
                semantic_hash(&format!("{base}[flags]\n{changed}")).unwrap(),
                "{changed} should change the runtime behavior"
            );
        }
    }

    #[test]
    fn disguise_policy_spelling_does_not_change_the_runtime_configuration() {
        let base = "instance_id='11111111-1111-1111-1111-111111111111'\n";
        for (mode, legacy) in [
            ("prefer", ""),
            ("off", "prefer_wss_http3_for_p2p = false\n"),
        ] {
            let unified = format!("{base}[flags]\np2p_disguise_mode = '{mode}'\n");
            let legacy = format!("{base}[flags]\n{legacy}");
            let active = TomlConfig::new_from_str(&legacy).unwrap();
            let persisted = TomlConfig::new_from_str(&unified).unwrap();
            assert!(configs_equivalent(&active, &persisted), "{mode}");
            assert_eq!(
                persisted.get_flags_patch().p2p_disguise_mode.as_deref(),
                Some(mode)
            );
        }
        let prefer = TomlConfig::new_from_str(base).unwrap();
        let off = TomlConfig::new_from_str(&format!("{base}[flags]\np2p_disguise_mode = 'off'\n"))
            .unwrap();
        assert!(!configs_equivalent(&prefer, &off));
    }

    #[test]
    fn semantic_hash_matches_active_flags_without_mutating_patch_presence() {
        let contents =
            "instance_id='11111111-1111-1111-1111-111111111111'\n[flags]\ndisable_p2p=false\n";
        let persisted = TomlConfig::new_from_str(contents).unwrap();
        let before_patch = persisted.get_flags_patch();
        let before_dump = persisted.dump();
        let active = TomlConfig::new_from_str(contents).unwrap();
        active.set_flags(active.get_flags());
        assert_ne!(before_patch, active.get_flags_patch());
        assert_eq!(
            semantic_hash(contents),
            config_semantic_hash(&active),
            "equivalence compares both configurations after resolving defaults"
        );
        assert!(configs_equivalent(&persisted, &active));
        for changed in ["close_redundant_conns_when_disguised = false", "mtu = 0"] {
            let changed = TomlConfig::new_from_str(&format!("{contents}{changed}\n")).unwrap();
            assert!(!configs_equivalent(&changed, &active));
        }
        assert_eq!(before_patch, persisted.get_flags_patch());
        assert_eq!(before_dump, persisted.dump());
        let projected = network_config_from_toml(&persisted);
        assert_eq!(projected.disable_p2p, Some(false));
        assert_eq!(projected.mtu, None);
        assert_eq!(projected.disable_encryption, None);
    }
}
