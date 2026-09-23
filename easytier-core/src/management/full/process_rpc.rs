use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use easytier_proto::{
    api::manage::{
        CollectNetworkInfoRequest, CollectNetworkInfoResponse, DeleteNetworkInstanceRequest,
        DeleteNetworkInstanceResponse, GetNetworkInstanceConfigRequest,
        GetNetworkInstanceConfigResponse, ListNetworkInstanceMetaRequest,
        ListNetworkInstanceMetaResponse, ListNetworkInstanceRequest, ListNetworkInstanceResponse,
        NetworkConfig, NetworkInstanceRunningInfoMap, NetworkMeta,
        RemoveNetworkInstanceConfigRequest, RemoveNetworkInstanceConfigResponse,
        RetainNetworkInstanceRequest, RetainNetworkInstanceResponse, RunNetworkInstanceRequest,
        RunNetworkInstanceResponse, SaveNetworkInstanceConfigRequest,
        SaveNetworkInstanceConfigResponse, SetNetworkInstanceEnabledRequest,
        SetNetworkInstanceEnabledResponse, ValidateConfigRequest, ValidateConfigResponse,
        WebClientService,
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
};

use super::config_state::InstanceStateStore;
use super::{
    ConfigFileControl, ConfigFilePermission, InstanceManager, config_source_from_rpc,
    config_source_to_rpc, network_instance_running_info,
};

#[async_trait::async_trait]
pub trait InstanceMutationHooks: Send + Sync + 'static {
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
}

#[async_trait::async_trait]
impl InstanceMutationHooks for () {}

/// Host Adapter for configuration-file effects used by process management.
#[async_trait::async_trait]
pub trait ConfigFileStorage: Send + Sync + 'static {
    async fn inspect(&self, path: &Path) -> ConfigFileControl;

    async fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>>;

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

/// Whether a config-file error means the file is already gone. Removal paths
/// use this to treat an externally deleted file as removed instead of failing.
fn is_not_found_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// Transport-independent process-level Instance management.
pub struct ProcessManagement<F>
where
    F: InstanceFactory,
{
    instances: Arc<InstanceManager<F>>,
    hooks: Arc<dyn InstanceMutationHooks>,
    storage: Arc<dyn ConfigFileStorage>,
    mutation_lock: Arc<tokio::sync::Mutex<()>>,
    state_store: Arc<InstanceStateStore>,
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
        if control.is_read_only() {
            anyhow::bail!("instance {instance_id} is read-only, cannot be overwritten");
        }
        if require_deletable && !control.is_deletable() {
            anyhow::bail!("instance {instance_id} is no-delete, cannot be overwritten");
        }
        if let Some(path) = control.path.as_deref()
            && self.storage.inspect(path).await.is_read_only()
        {
            anyhow::bail!(
                "config file {} is read-only, cannot be overwritten",
                path.display()
            );
        }
        Ok(())
    }

    async fn apply_file_cleanup(&self, cleanup: ConfigFileCleanup) {
        let result = match cleanup {
            ConfigFileCleanup::Remove(path) => self.storage.remove(&path).await,
            ConfigFileCleanup::Restore { path, contents } => {
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
        let _mutation = self.mutation_lock.lock().await;
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
        let remote_managed = self.hooks.manages_remote_config_instances();

        let mut replacing = false;
        let mut restore_instance = None;
        let mut control = if let Some(control) = self.instances.config_control(instance_id) {
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

        self.hooks
            .pre_run_network_instance(&config)
            .await
            .map_err(|error| anyhow::anyhow!("pre-run hook failed: {error}"))?;

        if replacing {
            self.ensure_overwritable(instance_id, &control, remote_managed)
                .await?;
        }

        let mut file_cleanup = None;
        if !control.is_read_only()
            && let Some(path) = control.path.as_deref()
        {
            let cleanup = match self.storage.read(path).await {
                Ok(Some(contents)) => Some(ConfigFileCleanup::Restore {
                    path: path.to_owned(),
                    contents,
                }),
                Ok(None) => Some(ConfigFileCleanup::Remove(path.to_owned())),
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "failed to back up config file {} before overwrite: {error}",
                        path.display()
                    ));
                }
            };
            if let Err(error) = self.storage.write(path, config.dump().as_bytes()).await {
                tracing::warn!(%error, path = %path.display(), "failed to write config file");
                control.set_read_only(true);
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

        if let Err(error) = self.instances.run_network_instance(config, control) {
            self.rollback_started_instance(None, file_cleanup, restore_instance)
                .await;
            return Err(error);
        }

        if let Err(error) = self.hooks.post_run_network_instance(&instance_id).await {
            if remote_managed {
                self.rollback_started_instance(Some(instance_id), file_cleanup, restore_instance)
                    .await;
                return Err(anyhow::anyhow!("post-run hook failed: {error}"));
            }
            tracing::warn!(%error, "post-run hook failed");
        }
        if let Err(error) = self.state_store.set_enabled(instance_id, true) {
            tracing::warn!(%error, %instance_id, "failed to persist enabled state");
        }
        Ok(instance_id)
    }

    pub async fn retain_network_instances(
        &self,
        retained: Vec<uuid::Uuid>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let _mutation = self.mutation_lock.lock().await;
        self.retain_network_instances_locked(retained).await
    }

    /// Resolves retained names and mutates the collection in one transaction.
    pub async fn retain_owned_network_instances_by_name(
        &self,
        retained_names: Vec<String>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let _mutation = self.mutation_lock.lock().await;
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
        let _mutation = self.mutation_lock.lock().await;
        let requested = requested.into_iter().collect::<HashSet<_>>();
        let remote_managed = self.hooks.manages_remote_config_instances();
        let mut removed = Vec::new();
        let mut files = Vec::new();
        for instance in self.instances.instances() {
            let instance_id = instance.instance_id();
            if !requested.contains(&instance_id) {
                continue;
            }
            let Some(control) = self.instances.config_control(instance_id) else {
                continue;
            };
            let removable = if remote_managed {
                self.is_remote_removable(&control).await
            } else {
                control.is_deletable()
            };
            if removable {
                removed.push(instance_id);
                files.extend(control.path);
            }
        }
        let remaining = self
            .instances
            .delete_network_instances(removed.clone())
            .await?;
        self.notify_removed_instances(&removed).await?;
        for instance_id in &removed {
            let _ = self.state_store.remove(instance_id);
        }
        for path in files {
            if remote_managed && self.storage.inspect(&path).await.is_read_only() {
                continue;
            }
            match self.storage.remove(&path).await {
                Ok(()) => {}
                Err(error) if is_not_found_error(&error) => {}
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to remove config file");
                }
            }
        }
        Ok(InstanceMutationResult {
            remaining_instance_ids: remaining,
            removed_instance_ids: removed,
        })
    }

    /// Persists a config file under the config dir without starting it and
    /// marks the instance as disabled (saved but not running).
    pub async fn save_network_instance_config(
        &self,
        config: TomlConfig,
        requested_id: Option<uuid::Uuid>,
        requested_source: Option<ConfigSource>,
    ) -> anyhow::Result<uuid::Uuid> {
        let mut instance_id = config.get_id();
        if let Some(requested_id) = requested_id {
            instance_id = requested_id;
            config.set_id(instance_id);
        }
        let _mutation = self.mutation_lock.lock().await;
        // Instance names must be unique among *running* instances only, which
        // `set_network_instance_enabled` enforces when a saved config starts.
        // Rejecting the save itself would block adding a second network that
        // happens to reuse a name (the GUI and web console derive the instance
        // name from the network name, so defaults collide easily).
        if self.instances.instance(instance_id).is_some() {
            // Running instance: persist the config and apply it immediately by
            // restarting the instance with the new config. The locked variant
            // is used because we already hold the mutation lock.
            return self
                .run_network_instance_locked(config, instance_id, true, requested_source)
                .await;
        }
        let Some(config_dir) = self.instances.config_dir() else {
            anyhow::bail!("config dir is not configured, cannot save config file");
        };
        let path = config_dir.join(format!("{instance_id}.toml"));
        let existing = self.storage.inspect(&path).await;
        if existing.is_read_only() {
            anyhow::bail!(
                "config file {} is read-only, cannot save config",
                path.display()
            );
        }
        config.set_network_config_source(requested_source.or(Some(ConfigSource::Web)));
        self.storage
            .write(&path, config.dump().as_bytes())
            .await
            .map_err(|error| anyhow::anyhow!("failed to save config file: {error}"))?;
        self.state_store.set_enabled(instance_id, false)?;
        Ok(instance_id)
    }

    /// Enables or disables a locally managed instance. Disabling stops the
    /// instance but keeps its config file; enabling restores it from the file.
    pub async fn set_network_instance_enabled(
        &self,
        instance_id: uuid::Uuid,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let _mutation = self.mutation_lock.lock().await;
        if enabled {
            if self.instances.instance(instance_id).is_some() {
                return Ok(());
            }
            let Some(config_dir) = self.instances.config_dir() else {
                anyhow::bail!("config dir is not configured, cannot restore instance");
            };
            let path = config_dir.join(format!("{instance_id}.toml"));
            let contents = self
                .storage
                .read(&path)
                .await
                .map_err(|error| anyhow::anyhow!("failed to read config file: {error}"))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "config file {} not found, cannot enable instance",
                        path.display()
                    )
                })?;
            let config_str = String::from_utf8(contents)
                .map_err(|error| anyhow::anyhow!("config file is not valid utf8: {error}"))?;
            let config =
                TomlConfig::new_from_str_with_source(&path.display().to_string(), &config_str)
                    .map_err(|error| {
                        anyhow::anyhow!("failed to parse config file {}: {error}", path.display())
                    })?;
            // Saving a config is unrestricted, but starting it is not: two
            // running instances must never share a name, and name lookups
            // (CLI, management API) assume the same.
            let instance_name = config.get_inst_name();
            if let Some(existing) =
                super::resolve_optional_instance_by_name(self.instances.as_ref(), &instance_name)?
                && existing.instance_id() != instance_id
            {
                anyhow::bail!(
                    "instance name {instance_name} is already used by a running instance; \
                     change the network name or stop that instance first"
                );
            }
            let control = self.storage.inspect(&path).await;
            self.instances.run_network_instance(config, control)?;
        } else {
            if self.instances.instance(instance_id).is_some() {
                self.instances
                    .delete_network_instances([instance_id])
                    .await?;
            }
        }
        self.state_store.set_enabled(instance_id, enabled)?;
        Ok(())
    }

    /// Removes the config files and state entries for the given instances.
    /// Running instances are left untouched.
    pub async fn remove_network_instance_configs(
        &self,
        requested: &[uuid::Uuid],
    ) -> anyhow::Result<Vec<uuid::Uuid>> {
        let _mutation = self.mutation_lock.lock().await;
        let mut removed = Vec::new();
        for instance_id in requested {
            let Some(config_dir) = self.instances.config_dir() else {
                continue;
            };
            let path = config_dir.join(format!("{instance_id}.toml"));
            let control = self.storage.inspect(&path).await;
            if !control.is_read_only() && control.is_deletable() {
                match self.storage.remove(&path).await {
                    Ok(()) => {}
                    // The file may already be gone (deleted by hand while the
                    // instance was stopped); its state entry is still stale and
                    // must be cleaned up.
                    Err(error) if is_not_found_error(&error) => {}
                    Err(error) => {
                        tracing::warn!(%error, path = %path.display(), "failed to remove config file");
                        continue;
                    }
                }
            }
            self.state_store.remove(instance_id)?;
            removed.push(*instance_id);
        }
        Ok(removed)
    }

    /// Reads a config from disk, falling back to the running instance config.
    pub async fn get_network_instance_config_from_file(
        &self,
        instance_id: uuid::Uuid,
    ) -> anyhow::Result<Option<(NetworkConfig, ConfigSource)>> {
        let Some(config_dir) = self.instances.config_dir() else {
            return Ok(None);
        };
        let path = config_dir.join(format!("{instance_id}.toml"));
        let contents = match self.storage.read(&path).await {
            Ok(Some(contents)) => contents,
            _ => return Ok(None),
        };
        let config_str = match String::from_utf8(contents) {
            Ok(config_str) => config_str,
            Err(_) => return Ok(None),
        };
        let config =
            match TomlConfig::new_from_str_with_source(&path.display().to_string(), &config_str) {
                Ok(config) => config,
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to parse config file");
                    return Ok(None);
                }
            };
        let source = config.get_network_config_source();
        Ok(Some((network_config_from_toml(&config), source)))
    }

    /// Starts one caller-owned Instance while preserving its static control.
    pub async fn run_owned_network_instance(
        &self,
        config: TomlConfig,
        control: ConfigFileControl,
    ) -> anyhow::Result<uuid::Uuid> {
        let _mutation = self.mutation_lock.lock().await;
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
        let _mutation = self.mutation_lock.lock().await;
        let requested = select();
        self.delete_owned_network_instances_locked(requested).await
    }

    /// Resolves requested names and removes them in one transaction.
    pub async fn delete_owned_network_instances_by_name(
        &self,
        requested_names: Vec<String>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let _mutation = self.mutation_lock.lock().await;
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
    Remove(PathBuf),
    Restore { path: PathBuf, contents: Vec<u8> },
}

/// Protobuf projection over transport-independent process management.
pub struct ProcessManagementRpc<F>
where
    F: InstanceFactory,
{
    management: ProcessManagement<F>,
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
        Ok(ListNetworkInstanceResponse {
            inst_ids: self
                .management
                .instances
                .instance_ids()
                .into_iter()
                .map(Into::into)
                .collect(),
            disabled_inst_ids: self
                .management
                .state_store
                .disabled_instance_ids()
                .into_iter()
                .map(Into::into)
                .collect(),
        })
    }

    async fn delete_network_instance(
        &self,
        _: BaseController,
        request: DeleteNetworkInstanceRequest,
    ) -> rpc_types::error::Result<DeleteNetworkInstanceResponse> {
        let requested = request.inst_ids.into_iter().map(Into::into).collect();
        let management = self.management.clone();
        let result =
            tokio::spawn(async move { management.delete_network_instances(requested).await })
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
        for instance_id in self.management.state_store.disabled_instance_ids() {
            // Only include disabled instances the caller explicitly asked for;
            // skip any disabled instance not in the request, so the response is
            // exactly the requested set (running ones above, disabled ones here).
            if !request_contains(&requested_ids, instance_id) {
                continue;
            }
            if let Some((config, source)) = self
                .management
                .get_network_instance_config_from_file(instance_id)
                .await?
            {
                let network_name = config.network_name.unwrap_or_default();
                metas.push(NetworkMeta {
                    inst_id: Some(instance_id.into()),
                    network_name: network_name.clone(),
                    config_permission: 0,
                    instance_name: network_name,
                    source: config_source_to_rpc(source),
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
        let config = request
            .config
            .ok_or_else(|| anyhow::anyhow!("config is required"))?
            .gen_config()?;
        let requested_id = request.inst_id.map(Into::into);
        let requested_source = config_source_from_rpc(request.source);
        let management = self.management.clone();
        tokio::spawn(async move {
            management
                .save_network_instance_config(config, requested_id, requested_source)
                .await
        })
        .await
        .map_err(|error| anyhow::anyhow!("instance mutation task failed: {error}"))??;
        Ok(SaveNetworkInstanceConfigResponse {})
    }

    async fn set_network_instance_enabled(
        &self,
        _: BaseController,
        request: SetNetworkInstanceEnabledRequest,
    ) -> rpc_types::error::Result<SetNetworkInstanceEnabledResponse> {
        let instance_id = request
            .inst_id
            .ok_or_else(|| anyhow::anyhow!("instance id is required"))?
            .into();
        let management = self.management.clone();
        tokio::spawn(async move {
            management
                .set_network_instance_enabled(instance_id, request.enabled)
                .await
        })
        .await
        .map_err(|error| anyhow::anyhow!("instance mutation task failed: {error}"))??;
        Ok(SetNetworkInstanceEnabledResponse {})
    }

    async fn remove_network_instance_config(
        &self,
        _: BaseController,
        request: RemoveNetworkInstanceConfigRequest,
    ) -> rpc_types::error::Result<RemoveNetworkInstanceConfigResponse> {
        let requested = request
            .inst_ids
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let to_remove = requested.clone();
        let management = self.management.clone();
        let removed =
            tokio::spawn(
                async move { management.remove_network_instance_configs(&to_remove).await },
            )
            .await
            .map_err(|error| anyhow::anyhow!("instance mutation task failed: {error}"))??;
        Ok(RemoveNetworkInstanceConfigResponse {
            remain_inst_ids: requested
                .into_iter()
                .filter(|id| !removed.contains(id))
                .map(Into::into)
                .collect(),
        })
    }
}

fn request_contains(inst_ids: &[easytier_proto::common::Uuid], instance_id: uuid::Uuid) -> bool {
    inst_ids
        .iter()
        .any(|id| uuid::Uuid::from(*id) == instance_id)
}
