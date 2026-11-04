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
};

use super::config_state::InstanceStateStore;
use super::{
    ConfigFileControl, ConfigFilePermission, InstanceManager, config_source_from_rpc,
    config_source_to_rpc, network_instance_running_info,
};

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

mod web_client;
