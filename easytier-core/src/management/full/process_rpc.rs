use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use easytier_proto::{
    api::manage::{
        CollectNetworkInfoRequest, CollectNetworkInfoResponse, DeleteNetworkInstanceRequest,
        DeleteNetworkInstanceResponse, GetNetworkInstanceConfigRequest,
        GetNetworkInstanceConfigResponse, ListNetworkInstanceMetaRequest,
        ListNetworkInstanceMetaResponse, ListNetworkInstanceRequest, ListNetworkInstanceResponse,
        NetworkInstanceRunningInfoMap, NetworkMeta, RemoveNetworkInstanceConfigRequest,
        RemoveNetworkInstanceConfigResponse, RetainNetworkInstanceRequest,
        RetainNetworkInstanceResponse, RunNetworkInstanceRequest, RunNetworkInstanceResponse,
        SaveNetworkInstanceConfigRequest, SaveNetworkInstanceConfigResponse,
        SetNetworkInstanceEnabledRequest, SetNetworkInstanceEnabledResponse, ValidateConfigRequest,
        ValidateConfigResponse, WebClientService,
    },
    rpc_types::{self, controller::BaseController},
};

use crate::{
    config::{
        api::network_config_from_toml,
        api_input::NetworkConfigExt as _,
        toml::{ConfigLoader as _, ConfigSource, TomlConfig},
    },
    instance::{CoreInstance, CoreInstanceHost, manager::InstanceFactory},
    management::LocalConfigMutationError,
};

use super::config_state::InstanceStateStore;
use super::{
    ConfigFileControl, ConfigFilePermission, InstanceManager, config_source_from_rpc,
    config_source_to_rpc, network_instance_running_info,
};

mod config_rpc;
mod lifecycle;

#[async_trait::async_trait]
pub trait InstanceMutationHooks: Send + Sync + 'static {
    /// Plans a start without calling back into process mutations.
    async fn prepare_start(
        &self,
        _config: &TomlConfig,
        _active: &[ActiveInstanceForStart],
    ) -> Result<Vec<uuid::Uuid>, String> {
        Ok(Vec::new())
    }

    fn manages_remote_config_instances(&self) -> bool {
        false
    }

    async fn pre_run_network_instance(&self, _config: &TomlConfig) -> Result<(), String> {
        Ok(())
    }

    async fn post_run_network_instance(&self, _instance_id: &uuid::Uuid) -> Result<(), String> {
        Ok(())
    }

    async fn post_remove_network_instances(
        &self,
        _instance_ids: &[uuid::Uuid],
    ) -> Result<(), String> {
        Ok(())
    }

    async fn post_stop_network_instances(
        &self,
        _instance_ids: &[uuid::Uuid],
    ) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ActiveInstanceForStart {
    pub instance_id: uuid::Uuid,
    pub no_tun: bool,
    pub source: ConfigSource,
}

#[derive(Default)]
pub(super) struct InstanceRunPersistence<'a> {
    pub contents: Option<&'a [u8]>,
    pub expected_revision: Option<&'a str>,
}

#[async_trait::async_trait]
impl InstanceMutationHooks for () {}

/// Host Adapter for configuration-file effects used by process management.
#[async_trait::async_trait]
pub trait ConfigFileStorage: Send + Sync + 'static {
    fn supports_catalog(&self) -> bool {
        false
    }

    /// Native file owners opt into applying external edits. Other Hosts can
    /// retain an observation-only catalog (for example an embedded config store).
    fn supports_live_reconcile(&self) -> bool {
        false
    }

    /// Keep the returned guard alive for as long as directory notifications
    /// are needed. Hosts without a watcher use the reconciliation poller.
    fn watch_configs(
        &self,
        _directory: &Path,
        _changed: Arc<dyn Fn() + Send + Sync>,
    ) -> anyhow::Result<Option<Box<dyn Send>>> {
        Ok(None)
    }

    async fn list_configs(&self, _directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
        anyhow::bail!("configuration observation is unsupported by this Host")
    }

    /// Inspect original bytes without environment expansion. Native Hosts may
    /// add stricter filesystem and environment-reference protection.
    async fn inspect_raw_config(
        &self,
        path: &Path,
        contents: &[u8],
        config_dir: &Path,
    ) -> anyhow::Result<ConfigFileControl> {
        let mut control = self.inspect(path).await;
        let identity = std::str::from_utf8(contents)
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(text).ok())
            .and_then(|table| {
                table
                    .get("instance_id")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
            })
            .and_then(|id| id.parse::<uuid::Uuid>().ok());
        let managed = path.parent() == Some(config_dir)
            && path.extension() == Some(std::ffi::OsStr::new("toml"))
            && identity.is_some_and(|id| {
                path.file_stem().and_then(|stem| stem.to_str()) == Some(id.to_string().as_str())
            });
        control.set_no_delete(control.is_read_only() || !managed);
        Ok(control)
    }

    async fn inspect(&self, path: &Path) -> ConfigFileControl;

    async fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>>;

    async fn load_config(
        &self,
        path: &Path,
        _config_dir: Option<&Path>,
    ) -> anyhow::Result<Option<(TomlConfig, ConfigFileControl)>> {
        let Some(contents) = self.read(path).await? else {
            return Ok(None);
        };
        let contents = String::from_utf8(contents)?;
        let config = TomlConfig::new_from_str_with_source(&path.display().to_string(), &contents)?;
        Ok(Some((config, self.inspect(path).await)))
    }

    /// Atomically replaces the file and restricts newly created files to the
    /// current user when the Host supports file permissions.
    async fn write(&self, path: &Path, contents: &[u8]) -> anyhow::Result<()>;

    async fn remove(&self, path: &Path) -> anyhow::Result<()>;
}

#[derive(Default)]
pub struct UnsupportedConfigFileStorage;

#[async_trait::async_trait]
impl ConfigFileStorage for UnsupportedConfigFileStorage {
    async fn inspect(&self, path: &Path) -> ConfigFileControl {
        ConfigFileControl::new(
            Some(path.to_owned()),
            ConfigFilePermission::from(ConfigFilePermission::READ_ONLY),
        )
    }

    async fn read(&self, _path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        anyhow::bail!("configuration-file storage is unsupported by this Host")
    }

    async fn write(&self, _path: &Path, _contents: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("configuration-file storage is unsupported by this Host")
    }

    async fn remove(&self, _path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("configuration-file storage is unsupported by this Host")
    }
}

/// Result of one process-level removal transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceMutationResult {
    pub remaining_instance_ids: Vec<uuid::Uuid>,
    pub removed_instance_ids: Vec<uuid::Uuid>,
}

/// Transport-independent process-level Instance management.
pub struct ProcessManagement<F>
where
    F: InstanceFactory,
{
    pub(super) instances: Arc<InstanceManager<F>>,
    pub(super) hooks: Arc<dyn InstanceMutationHooks>,
    pub(super) storage: Arc<dyn ConfigFileStorage>,
    pub(super) mutation_lock: Arc<tokio::sync::Mutex<()>>,
    pub(super) state_store: Arc<InstanceStateStore>,
}

impl<F> Clone for ProcessManagement<F>
where
    F: InstanceFactory,
{
    fn clone(&self) -> Self {
        Self {
            instances: self.instances.clone(),
            hooks: self.hooks.clone(),
            storage: self.storage.clone(),
            mutation_lock: self.mutation_lock.clone(),
            state_store: self.state_store.clone(),
        }
    }
}

impl<F, H> ProcessManagement<F>
where
    F: InstanceFactory<Instance = CoreInstance<H>, CreateContext = ()>,
    F::Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
    H: CoreInstanceHost,
{
    pub fn new(
        instances: Arc<InstanceManager<F>>,
        hooks: Arc<dyn InstanceMutationHooks>,
        storage: Arc<dyn ConfigFileStorage>,
        state_store: Arc<InstanceStateStore>,
    ) -> Self {
        let mutation_lock = instances.mutation_lock();
        #[cfg(feature = "management")]
        let storage = super::local_reconcile::coordinated_storage(&instances, storage);
        #[cfg(feature = "management")]
        let state_store = if instances.config_dir().is_some() && storage.supports_catalog() {
            instances
                .local_config_catalog()
                .shared_state_store(state_store)
        } else {
            state_store
        };
        #[cfg(feature = "management")]
        super::local_catalog::start_observer(
            &instances,
            hooks.clone(),
            storage.clone(),
            state_store.clone(),
        );
        Self {
            instances,
            hooks,
            storage,
            mutation_lock,
            state_store,
        }
    }

    /// Instance-level enabled/disabled state store, shared with the caller.
    pub fn state_store(&self) -> Arc<InstanceStateStore> {
        self.state_store.clone()
    }

    async fn is_remote_removable(&self, control: &ConfigFileControl) -> bool {
        if control.is_read_only() || !control.is_deletable() {
            return false;
        }
        let Some(path) = control.path.as_deref() else {
            return true;
        };
        !self.storage.inspect(path).await.is_read_only()
    }

    async fn ensure_overwritable(
        &self,
        instance_id: uuid::Uuid,
        control: &ConfigFileControl,
        require_deletable: bool,
    ) -> anyhow::Result<()> {
        #[cfg(feature = "management")]
        self.ensure_catalog_identity_locked(instance_id).await?;
        if control.is_read_only() {
            return Err(
                anyhow::Error::from(LocalConfigMutationError::ReadOnly).context(format!(
                    "instance {instance_id} is read-only, cannot be overwritten"
                )),
            );
        }
        if require_deletable && !control.is_deletable() {
            anyhow::bail!("instance {instance_id} is no-delete, cannot be overwritten");
        }
        if let Some(path) = control.path.as_deref() {
            let loaded = self
                .storage
                .load_config(path, self.instances.config_dir().map(PathBuf::as_path))
                .await?;
            if loaded
                .as_ref()
                .is_some_and(|(config, _)| config.get_id() != instance_id)
            {
                anyhow::bail!(
                    "config file {} does not contain instance {instance_id}",
                    path.display()
                );
            }
            let current = match loaded {
                Some((_, control)) => control,
                None => self.storage.inspect(path).await,
            };
            if current.is_read_only() || (require_deletable && !current.is_deletable()) {
                anyhow::bail!(
                    "config file {} is protected, cannot be overwritten",
                    path.display()
                );
            }
        }
        Ok(())
    }

    async fn apply_file_cleanup(&self, cleanup: ConfigFileCleanup) {
        let (path, written) = match &cleanup {
            ConfigFileCleanup::Remove { path, written }
            | ConfigFileCleanup::Restore { path, written, .. } => (path, written),
        };
        if self.storage.read(path).await.ok().flatten().as_deref() != Some(written.as_slice()) {
            tracing::warn!("configuration changed externally; preserving it during rollback");
            return;
        }
        let directory = self
            .instances
            .config_dir()
            .map(PathBuf::as_path)
            .or_else(|| path.parent())
            .unwrap_or(Path::new(""));
        let control = self
            .storage
            .inspect_raw_config(path, written, directory)
            .await;
        let protected = match control {
            Ok(control) => {
                control.is_read_only()
                    || (matches!(&cleanup, ConfigFileCleanup::Remove { .. })
                        && !control.is_deletable())
            }
            Err(_) => true,
        };
        if protected {
            tracing::warn!("configuration became protected; preserving it during rollback");
            return;
        }
        let result = match cleanup {
            ConfigFileCleanup::Remove { path, .. } => self.storage.remove(&path).await,
            ConfigFileCleanup::Restore { path, contents, .. } => {
                self.storage.write(&path, &contents).await
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, "failed to roll back configuration file");
        }
    }

    async fn rollback_started_instance(
        &self,
        started_instance_id: Option<uuid::Uuid>,
        file_cleanup: Option<ConfigFileCleanup>,
        restore_instance: Option<(TomlConfig, ConfigFileControl)>,
    ) {
        if let Some(instance_id) = started_instance_id
            && let Err(error) = self.instances.delete_network_instances([instance_id]).await
        {
            tracing::warn!(%error, "failed to remove rolled-back instance");
            return;
        }
        if let Some(cleanup) = file_cleanup {
            self.apply_file_cleanup(cleanup).await;
        }
        if let Some((config, control)) = restore_instance
            && let Err(error) = self.instances.run_network_instance(config, control)
        {
            tracing::warn!(%error, "failed to restore overwritten instance");
        }
    }

    pub async fn run_network_instance(
        &self,
        config: TomlConfig,
        requested_id: Option<uuid::Uuid>,
        overwrite: bool,
        requested_source: Option<ConfigSource>,
    ) -> anyhow::Result<uuid::Uuid> {
        let mut instance_id = config.get_id();
        if let Some(requested_id) = requested_id {
            instance_id = requested_id;
            config.set_id(instance_id);
        }
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        self.run_network_instance_locked(config, instance_id, overwrite, requested_source)
            .await
    }

    /// Core of [`run_network_instance`] assuming the caller already holds the
    /// mutation lock. Used by `save_network_instance_config` to apply a saved
    /// config to a running instance immediately without re-acquiring the lock.
    async fn run_network_instance_locked(
        &self,
        config: TomlConfig,
        instance_id: uuid::Uuid,
        overwrite: bool,
        requested_source: Option<ConfigSource>,
    ) -> anyhow::Result<uuid::Uuid> {
        self.run_network_instance_locked_with_contents(
            config,
            instance_id,
            overwrite,
            requested_source,
            InstanceRunPersistence::default(),
        )
        .await
    }

    pub(super) async fn run_network_instance_locked_with_contents(
        &self,
        config: TomlConfig,
        instance_id: uuid::Uuid,
        overwrite: bool,
        requested_source: Option<ConfigSource>,
        persistence: InstanceRunPersistence<'_>,
    ) -> anyhow::Result<uuid::Uuid> {
        let InstanceRunPersistence {
            contents: persisted_contents,
            expected_revision,
        } = persistence;
        let remote_managed = self.hooks.manages_remote_config_instances();
        let previous_enabled = self.state_store.enabled_state(&instance_id);
        self.ensure_config_identity(instance_id)?;

        let mut replacing = false;
        let mut restore_instance = None;
        let control = if let Some(control) = self.instances.config_control(instance_id) {
            let existing_source = self.instances.config_source(instance_id);
            let error_message = self
                .instances
                .network_info(instance_id)
                .await
                .and_then(|info| info.error_msg)
                .unwrap_or_default();
            if !overwrite && error_message.is_empty() {
                return Ok(instance_id);
            }
            self.ensure_overwritable(instance_id, &control, remote_managed)
                .await?;
            config.set_network_config_source(requested_source.or(existing_source));
            replacing = true;
            restore_instance = self
                .instances
                .config(instance_id)
                .map(|config| (config, control.clone()));
            control
        } else if let Some(control) = self.instances.managed_config_control(instance_id) {
            self.ensure_overwritable(instance_id, &control, remote_managed)
                .await?;
            config.set_network_config_source(requested_source);
            control
        } else if let Some(config_dir) = self.instances.config_dir() {
            config.set_network_config_source(requested_source);
            ConfigFileControl::new(
                Some(config_dir.join(format!("{instance_id}.toml"))),
                ConfigFilePermission::default(),
            )
        } else {
            config.set_network_config_source(requested_source);
            ConfigFileControl::new(None, ConfigFilePermission::default())
        };

        self.ensure_overwritable(instance_id, &control, remote_managed)
            .await?;
        #[cfg(feature = "management")]
        let legacy_candidate = config.detached_snapshot();
        #[cfg(feature = "management")]
        if persisted_contents.is_none()
            && let Some(path) = control.path.as_deref()
            && let Some(original) = self.storage.read(path).await?
        {
            let text = std::str::from_utf8(&original)
                .map_err(|_| anyhow::anyhow!("invalid persisted configuration"))?;
            let merged =
                super::persisted_merge::merge_legacy_form_changes(text, &legacy_candidate)?;
            let merged_config = TomlConfig::new_from_str(&merged)
                .map_err(|_| anyhow::anyhow!("invalid persisted configuration"))?;
            config.replace_from_snapshot(&merged_config);
            super::local_catalog::reuse_implicit_identity(
                merged.as_bytes(),
                &config,
                self.instances.config(instance_id).as_ref(),
            );
        }
        #[cfg(feature = "management")]
        if let Some(contents) = persisted_contents {
            super::local_catalog::reuse_implicit_identity(
                contents,
                &config,
                self.instances.config(instance_id).as_ref(),
            );
        }
        if !replacing {
            self.state_store.set_enabled(instance_id, false)?;
        }
        if let Err(error) = self.prepare_start_locked(&config).await {
            if !replacing {
                self.restore_enabled_state(instance_id, previous_enabled)?;
            }
            return Err(error);
        }
        if let Err(error) = self.hooks.pre_run_network_instance(&config).await {
            if !replacing {
                self.restore_enabled_state(instance_id, previous_enabled)?;
            }
            return Err(anyhow::anyhow!("pre-run hook failed: {error}"));
        }

        if replacing {
            self.ensure_overwritable(instance_id, &control, remote_managed)
                .await?;
        }

        let mut file_cleanup = None;
        if !control.is_read_only()
            && let Some(path) = control.path.as_deref()
        {
            #[cfg(feature = "management")]
            self.verify_local_revision_locked(instance_id, expected_revision)
                .await?;
            let original = self.storage.read(path).await;
            let mut written = config.dump().into_bytes();
            #[cfg(feature = "management")]
            if persisted_contents.is_none()
                && let Ok(Some(original)) = &original
            {
                let merged = std::str::from_utf8(original)
                    .map_err(|_| anyhow::anyhow!("invalid persisted configuration"))
                    .and_then(|text| {
                        super::persisted_merge::merge_legacy_form_changes(text, &legacy_candidate)
                    })
                    .and_then(|merged| {
                        let parsed = TomlConfig::new_from_str(&merged)
                            .map_err(|_| anyhow::anyhow!("invalid persisted configuration"))?;
                        Ok((merged, parsed))
                    });
                match merged {
                    Ok((merged, merged_config)) => {
                        config.replace_from_snapshot(&merged_config);
                        super::local_catalog::reuse_implicit_identity(
                            merged.as_bytes(),
                            &config,
                            self.instances.config(instance_id).as_ref(),
                        );
                        written = merged.into_bytes();
                    }
                    Err(error) => {
                        if !replacing {
                            self.restore_enabled_state(instance_id, previous_enabled)?;
                        }
                        return Err(error);
                    }
                }
            }
            if let Some(contents) = persisted_contents {
                written = contents.to_vec();
            }
            let cleanup = match original {
                Ok(Some(contents)) => Some(ConfigFileCleanup::Restore {
                    path: path.to_owned(),
                    contents,
                    written: written.clone(),
                }),
                Ok(None) => Some(ConfigFileCleanup::Remove {
                    path: path.to_owned(),
                    written: written.clone(),
                }),
                Err(error) => {
                    if !replacing {
                        self.restore_enabled_state(instance_id, previous_enabled)?;
                    }
                    return Err(anyhow::anyhow!(
                        "failed to back up config file {} before overwrite: {error}",
                        path.display()
                    ));
                }
            };
            if let Err(error) = self.storage.write(path, &written).await {
                if !replacing {
                    self.restore_enabled_state(instance_id, previous_enabled)?;
                }
                return Err(anyhow::anyhow!(
                    "failed to write config file {}: {error}",
                    path.display()
                ));
            } else {
                file_cleanup = cleanup;
            }
        }

        if replacing
            && let Err(error) = self.instances.delete_network_instances([instance_id]).await
        {
            self.rollback_started_instance(None, file_cleanup, restore_instance)
                .await;
            return Err(error);
        }

        if let Err(error) = self.instances.run_network_instance(config, control.clone()) {
            self.rollback_started_instance(None, file_cleanup, restore_instance)
                .await;
            if !replacing {
                self.restore_enabled_state(instance_id, previous_enabled)?;
            }
            return Err(error);
        }

        if let Err(error) = self.hooks.post_run_network_instance(&instance_id).await {
            let restoring = restore_instance.is_some();
            let cleanup = if restoring { file_cleanup } else { None };
            self.rollback_started_instance(Some(instance_id), cleanup, restore_instance)
                .await;
            if !restoring {
                if control.path.as_deref().and_then(Path::parent)
                    == self.instances.config_dir().map(PathBuf::as_path)
                    && control.path.is_some()
                {
                    self.instances
                        .register_managed_config(instance_id, control)?;
                } else {
                    self.restore_enabled_state(instance_id, previous_enabled)?;
                }
                let _ = self.hooks.post_stop_network_instances(&[instance_id]).await;
            }
            return Err(anyhow::anyhow!("post-run hook failed: {error}"));
        }
        if control.path.as_deref().and_then(Path::parent)
            == self.instances.config_dir().map(PathBuf::as_path)
            && control.path.is_some()
        {
            self.instances
                .register_managed_config(instance_id, control.clone())?;
        }
        if let Err(error) = self.state_store.set_enabled(instance_id, true) {
            let restoring = restore_instance.is_some();
            self.rollback_started_instance(
                Some(instance_id),
                if restoring { file_cleanup } else { None },
                restore_instance,
            )
            .await;
            if !restoring {
                let _ = self.hooks.post_stop_network_instances(&[instance_id]).await;
                if control.path.is_some() && self.instances.config_dir().is_some() {
                    let _ = self.state_store.set_enabled(instance_id, false);
                } else {
                    let _ = self.restore_enabled_state(instance_id, previous_enabled);
                }
            }
            return Err(error);
        }
        Ok(instance_id)
    }

    pub async fn retain_network_instances(
        &self,
        retained: Vec<uuid::Uuid>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        self.retain_network_instances_locked(retained).await
    }

    /// Resolves retained names and mutates the collection in one transaction.
    pub async fn retain_owned_network_instances_by_name(
        &self,
        retained_names: Vec<String>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        let retained = self.resolve_instance_ids_by_name(&retained_names)?;
        self.retain_network_instances_locked(retained).await
    }

    async fn retain_network_instances_locked(
        &self,
        retained: Vec<uuid::Uuid>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let before = self.instances.instance_ids();
        if !self.hooks.manages_remote_config_instances() {
            let remaining = self.instances.retain_network_instances(&retained).await?;
            let remaining_set = remaining.iter().copied().collect::<HashSet<_>>();
            let removed = before
                .into_iter()
                .filter(|id| !remaining_set.contains(id))
                .collect::<Vec<_>>();
            self.notify_removed_instances(&removed).await?;
            return Ok(InstanceMutationResult {
                removed_instance_ids: removed,
                remaining_instance_ids: remaining,
            });
        }

        let mut retained = retained.into_iter().collect::<HashSet<_>>();
        let mut removed = Vec::new();
        for instance in self.instances.instances() {
            let instance_id = instance.instance_id();
            if retained.contains(&instance_id) {
                continue;
            }
            let Some(control) = self.instances.config_control(instance_id) else {
                continue;
            };
            if self.is_remote_removable(&control).await {
                removed.push(instance_id);
            } else {
                retained.insert(instance_id);
            }
        }
        let remaining = self
            .instances
            .retain_network_instances(&retained.into_iter().collect::<Vec<_>>())
            .await?;
        self.notify_removed_instances(&removed).await?;
        Ok(InstanceMutationResult {
            remaining_instance_ids: remaining,
            removed_instance_ids: removed,
        })
    }

    pub async fn delete_network_instances(
        &self,
        requested: Vec<uuid::Uuid>,
    ) -> anyhow::Result<InstanceMutationResult> {
        self.delete_network_instances_with_revision(requested, None)
            .await
    }

    /// Starts one caller-owned Instance while preserving its static control.
    pub async fn run_owned_network_instance(
        &self,
        config: TomlConfig,
        control: ConfigFileControl,
    ) -> anyhow::Result<uuid::Uuid> {
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        let instance_id = config.get_id();
        if self.instances.instance(instance_id).is_some() {
            anyhow::bail!("instance {instance_id} already exists");
        }
        let instance_name = config.get_inst_name();
        if super::resolve_optional_instance_by_name(self.instances.as_ref(), &instance_name)?
            .is_some()
        {
            anyhow::bail!("instance name {instance_name} already exists");
        }
        self.instances.run_network_instance(config, control)
    }

    /// Removes caller-owned Instances without applying remote config-file policy.
    pub async fn delete_owned_network_instances(
        &self,
        requested: Vec<uuid::Uuid>,
    ) -> anyhow::Result<InstanceMutationResult> {
        self.delete_owned_network_instances_selected_by(|| requested)
            .await
    }

    /// Selects caller-owned Instances and removes them under one lock.
    pub async fn delete_owned_network_instances_selected_by(
        &self,
        select: impl FnOnce() -> Vec<uuid::Uuid>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        let requested = select();
        self.delete_owned_network_instances_locked(requested).await
    }

    /// Resolves requested names and removes them in one transaction.
    pub async fn delete_owned_network_instances_by_name(
        &self,
        requested_names: Vec<String>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        let requested = self.resolve_instance_ids_by_name(&requested_names)?;
        self.delete_owned_network_instances_locked(requested).await
    }

    async fn delete_owned_network_instances_locked(
        &self,
        requested: Vec<uuid::Uuid>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let before = self.instances.instance_ids();
        let remaining = self.instances.delete_network_instances(requested).await?;
        let remaining_set = remaining.iter().copied().collect::<HashSet<_>>();
        let removed = before
            .into_iter()
            .filter(|id| !remaining_set.contains(id))
            .collect::<Vec<_>>();
        self.notify_removed_instances(&removed).await?;
        Ok(InstanceMutationResult {
            removed_instance_ids: removed,
            remaining_instance_ids: remaining,
        })
    }

    fn resolve_instance_ids_by_name(&self, names: &[String]) -> anyhow::Result<Vec<uuid::Uuid>> {
        names
            .iter()
            .map(|name| {
                super::resolve_optional_instance_by_name(self.instances.as_ref(), name)
                    .map(|instance| instance.map(|instance| instance.instance_id()))
            })
            .filter_map(Result::transpose)
            .collect()
    }

    async fn notify_removed_instances(&self, removed: &[uuid::Uuid]) -> anyhow::Result<()> {
        if let Err(error) = self.hooks.post_remove_network_instances(removed).await {
            if self.hooks.manages_remote_config_instances() {
                anyhow::bail!("post-remove hook failed: {error}");
            }
            tracing::warn!(%error, "post-remove hook failed");
        }
        Ok(())
    }
}

enum ConfigFileCleanup {
    Remove {
        path: PathBuf,
        written: Vec<u8>,
    },
    Restore {
        path: PathBuf,
        contents: Vec<u8>,
        written: Vec<u8>,
    },
}

/// Protobuf projection over transport-independent process management.
pub struct ProcessManagementRpc<F>
where
    F: InstanceFactory,
{
    pub(super) management: ProcessManagement<F>,
}

impl<F> Clone for ProcessManagementRpc<F>
where
    F: InstanceFactory,
{
    fn clone(&self) -> Self {
        Self {
            management: self.management.clone(),
        }
    }
}

impl<F, H> ProcessManagementRpc<F>
where
    F: InstanceFactory<Instance = CoreInstance<H>, CreateContext = ()>,
    F::Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
    H: CoreInstanceHost,
{
    pub fn new(
        instances: Arc<InstanceManager<F>>,
        hooks: Arc<dyn InstanceMutationHooks>,
        storage: Arc<dyn ConfigFileStorage>,
        state_store: Arc<InstanceStateStore>,
    ) -> Self {
        Self {
            management: ProcessManagement::new(instances, hooks, storage, state_store),
        }
    }
}

#[async_trait::async_trait]
impl<F, H> WebClientService for ProcessManagementRpc<F>
where
    F: InstanceFactory<Instance = CoreInstance<H>, CreateContext = ()>,
    F::Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
    H: CoreInstanceHost,
{
    type Controller = BaseController;

    async fn validate_config(
        &self,
        _: BaseController,
        request: ValidateConfigRequest,
    ) -> rpc_types::error::Result<ValidateConfigResponse> {
        Ok(ValidateConfigResponse {
            toml_config: request.config.unwrap_or_default().gen_config()?.dump(),
        })
    }

    async fn run_network_instance(
        &self,
        _: BaseController,
        request: RunNetworkInstanceRequest,
    ) -> rpc_types::error::Result<RunNetworkInstanceResponse> {
        let config = request
            .config
            .ok_or_else(|| anyhow::anyhow!("config is required"))?
            .gen_config()?;
        let requested_id = request.inst_id.map(Into::into);
        let requested_source = config_source_from_rpc(request.source);
        let management = self.management.clone();
        let instance_id = tokio::spawn(async move {
            management
                .run_network_instance(config, requested_id, request.overwrite, requested_source)
                .await
        })
        .await
        .map_err(|error| anyhow::anyhow!("instance mutation task failed: {error}"))??;
        Ok(RunNetworkInstanceResponse {
            inst_id: Some(instance_id.into()),
        })
    }

    async fn retain_network_instance(
        &self,
        _: BaseController,
        request: RetainNetworkInstanceRequest,
    ) -> rpc_types::error::Result<RetainNetworkInstanceResponse> {
        let retained = request.inst_ids.into_iter().map(Into::into).collect();
        let management = self.management.clone();
        let result =
            tokio::spawn(async move { management.retain_network_instances(retained).await })
                .await
                .map_err(|error| anyhow::anyhow!("instance mutation task failed: {error}"))??;
        Ok(RetainNetworkInstanceResponse {
            remain_inst_ids: result
                .remaining_instance_ids
                .into_iter()
                .map(Into::into)
                .collect(),
        })
    }

    async fn collect_network_info(
        &self,
        _: BaseController,
        request: CollectNetworkInfoRequest,
    ) -> rpc_types::error::Result<CollectNetworkInfoResponse> {
        let included = request
            .inst_ids
            .into_iter()
            .map(uuid::Uuid::from)
            .collect::<HashSet<_>>();
        let map = if included.is_empty() {
            self.management
                .instances
                .collect_network_infos()
                .await?
                .into_iter()
                .map(|(id, info)| (id.to_string(), info))
                .collect()
        } else {
            let mut map = std::collections::BTreeMap::new();
            for instance_id in included {
                let Some(instance) = self.management.instances.instance(instance_id) else {
                    continue;
                };
                map.insert(
                    instance_id.to_string(),
                    network_instance_running_info(instance.as_ref()).await?,
                );
            }
            map
        };
        Ok(CollectNetworkInfoResponse {
            info: Some(NetworkInstanceRunningInfoMap { map }),
        })
    }

    async fn list_network_instance(
        &self,
        _: BaseController,
        _: ListNetworkInstanceRequest,
    ) -> rpc_types::error::Result<ListNetworkInstanceResponse> {
        #[cfg(feature = "management")]
        let local_disabled = if self.management.storage.supports_live_reconcile()
            && self.management.instances.config_dir().is_some()
        {
            let _coordinator = self.management.lock_mutation().await?;
            let snapshot = self.management.refresh_local_configs_locked().await?;
            Some(
                snapshot
                    .entries
                    .into_iter()
                    .filter(|entry| !entry.running)
                    .filter_map(|entry| entry.inst_id)
                    .map(uuid::Uuid::from)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let disabled_inst_ids = self
            .management
            .state_store
            .disabled_instance_ids()
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(Into::into)
            .collect();
        #[cfg(feature = "management")]
        let disabled_inst_ids = local_disabled.unwrap_or(disabled_inst_ids);
        Ok(ListNetworkInstanceResponse {
            inst_ids: self
                .management
                .instances
                .instance_ids()
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(Into::into)
                .collect(),
            disabled_inst_ids,
            supports_persisted_config_management: Some(true),
            runtime_capabilities: self.management.instances.management_capabilities(),
        })
    }

    async fn delete_network_instance(
        &self,
        _: BaseController,
        request: DeleteNetworkInstanceRequest,
    ) -> rpc_types::error::Result<DeleteNetworkInstanceResponse> {
        let requested = request.inst_ids.into_iter().map(Into::into).collect();
        let management = self.management.clone();
        let result = tokio::spawn(async move {
            management
                .delete_network_instances_with_revision(requested, request.expected_revision)
                .await
        })
        .await
        .map_err(|error| anyhow::anyhow!("instance mutation task failed: {error}"))??;
        Ok(DeleteNetworkInstanceResponse {
            remain_inst_ids: result
                .remaining_instance_ids
                .into_iter()
                .map(Into::into)
                .collect(),
        })
    }

    async fn get_network_instance_config(
        &self,
        _: BaseController,
        request: GetNetworkInstanceConfigRequest,
    ) -> rpc_types::error::Result<GetNetworkInstanceConfigResponse> {
        let instance_id = request
            .inst_id
            .ok_or_else(|| anyhow::anyhow!("instance id is required"))?
            .into();
        #[cfg(feature = "management")]
        if self.management.storage.supports_catalog()
            && let Some((config, source)) = self
                .management
                .get_network_instance_config_from_file(instance_id)
                .await?
        {
            return Ok(GetNetworkInstanceConfigResponse {
                config: Some(config),
                source: config_source_to_rpc(source),
            });
        }
        if let Some(control) = self.management.instances.config_control(instance_id) {
            if control.is_read_only() {
                return Err(anyhow::anyhow!(
                    "configuration for instance {instance_id} is read-only"
                )
                .into());
            }
            return Ok(GetNetworkInstanceConfigResponse {
                config: self
                    .management
                    .instances
                    .config(instance_id)
                    .map(|config| network_config_from_toml(&config)),
                source: config_source_to_rpc(
                    self.management
                        .instances
                        .config_source(instance_id)
                        .unwrap_or(ConfigSource::User),
                ),
            });
        }
        let (config, source) = self
            .management
            .get_network_instance_config_from_file(instance_id)
            .await?
            .ok_or_else(|| {
                rpc_types::error::Error::from(anyhow::anyhow!(
                    "No such network instance: {instance_id}"
                ))
            })?;
        Ok(GetNetworkInstanceConfigResponse {
            config: Some(config),
            source: config_source_to_rpc(source),
        })
    }

    async fn list_network_instance_meta(
        &self,
        _: BaseController,
        request: ListNetworkInstanceMetaRequest,
    ) -> rpc_types::error::Result<ListNetworkInstanceMetaResponse> {
        let requested_ids = request.inst_ids.clone();
        let mut metas = Vec::with_capacity(requested_ids.len());
        for instance_id in requested_ids.iter().cloned().map(uuid::Uuid::from) {
            let Some(instance) = self.management.instances.instance(instance_id) else {
                continue;
            };
            let Some(config) = instance.toml_config() else {
                continue;
            };
            let Some(control) = self.management.instances.config_control(instance_id) else {
                continue;
            };
            metas.push(NetworkMeta {
                inst_id: Some(instance_id.into()),
                network_name: config.get_network_identity().network_name,
                config_permission: control.permission.into(),
                instance_name: instance.instance_name().to_owned(),
                source: config_source_to_rpc(config.get_network_config_source()),
            });
        }
        let stopped_ids = self.management.state_store.disabled_instance_ids();
        #[cfg(feature = "management")]
        let stopped_ids = if self.management.storage.supports_live_reconcile() {
            stopped_ids
                .into_iter()
                .chain(self.management.instances.managed_config_ids())
                .filter(|id| self.management.instances.instance(*id).is_none())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            stopped_ids
        };
        for instance_id in stopped_ids {
            // Only include disabled instances the caller explicitly asked for;
            // skip any disabled instance not in the request, so the response is
            // exactly the requested set (running ones above, disabled ones here).
            if !request_contains(&requested_ids, instance_id) {
                continue;
            }
            let loaded = match self.management.load_saved_config(instance_id).await {
                Ok(loaded) => loaded,
                Err(error) => {
                    tracing::warn!(%error, %instance_id, "failed to read disabled instance metadata");
                    continue;
                }
            };
            if let Some((config, control)) = loaded {
                let network_name = config.get_network_identity().network_name;
                metas.push(NetworkMeta {
                    inst_id: Some(instance_id.into()),
                    network_name: network_name.clone(),
                    config_permission: control.permission.into(),
                    instance_name: config.get_inst_name(),
                    source: config_source_to_rpc(config.get_network_config_source()),
                });
            }
        }
        Ok(ListNetworkInstanceMetaResponse { metas })
    }

    async fn save_network_instance_config(
        &self,
        _: BaseController,
        request: SaveNetworkInstanceConfigRequest,
    ) -> rpc_types::error::Result<SaveNetworkInstanceConfigResponse> {
        self.save_persisted_config(request).await
    }

    async fn set_network_instance_enabled(
        &self,
        _: BaseController,
        request: SetNetworkInstanceEnabledRequest,
    ) -> rpc_types::error::Result<SetNetworkInstanceEnabledResponse> {
        self.set_persisted_config_enabled(request).await
    }

    async fn remove_network_instance_config(
        &self,
        _: BaseController,
        request: RemoveNetworkInstanceConfigRequest,
    ) -> rpc_types::error::Result<RemoveNetworkInstanceConfigResponse> {
        self.remove_persisted_configs(request).await
    }
}

fn request_contains(inst_ids: &[easytier_proto::common::Uuid], instance_id: uuid::Uuid) -> bool {
    inst_ids
        .iter()
        .any(|id| uuid::Uuid::from(*id) == instance_id)
}
