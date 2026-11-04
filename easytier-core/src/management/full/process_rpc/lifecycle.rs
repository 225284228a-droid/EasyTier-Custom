//! Configuration-directory lifecycle operations for process-level Instance
//! management.
//!
//! These are inherent methods on `ProcessManagement`; keeping them in a child
//! module trims the transport-independent file without changing visibility,
//! lock order, or error handling.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use easytier_proto::api::manage::NetworkConfig;

use crate::{
    config::{
        api::network_config_from_toml,
        toml::{ConfigLoader as _, ConfigSource, TomlConfig},
    },
    instance::{CoreInstance, CoreInstanceHost, manager::InstanceFactory},
    management::LocalConfigMutationError,
};

#[cfg(feature = "management")]
use super::super::{config_source_from_rpc, local_catalog, persisted_merge};
use super::{
    ActiveInstanceForStart, ConfigFileControl, ConfigFilePermission, InstanceMutationResult,
    ProcessManagement,
};

/// Whether a config-file error means the file is already gone. Removal paths
/// use this to treat an externally deleted file as removed instead of failing.
fn is_not_found_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

impl<F, H> ProcessManagement<F>
where
    F: InstanceFactory<Instance = CoreInstance<H>, CreateContext = ()>,
    F::Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
    H: CoreInstanceHost,
{
    pub(crate) async fn lock_mutation(&self) -> anyhow::Result<tokio::sync::MutexGuard<'_, ()>> {
        let guard = self.mutation_lock.lock().await;
        #[cfg(feature = "management")]
        anyhow::ensure!(
            !self.instances.local_config_catalog().is_retired(),
            "instance manager has retired; reconnect to the current mode"
        );
        Ok(guard)
    }

    pub(crate) fn restore_enabled_state(
        &self,
        instance_id: uuid::Uuid,
        previous: Option<bool>,
    ) -> anyhow::Result<()> {
        match previous {
            Some(enabled) => self.state_store.set_enabled(instance_id, enabled),
            None => self.state_store.remove(&instance_id),
        }
    }

    fn saved_config_path(&self, instance_id: uuid::Uuid) -> Option<PathBuf> {
        self.instances
            .config_control(instance_id)
            .and_then(|control| control.path)
            .or_else(|| {
                self.instances
                    .managed_config_control(instance_id)
                    .and_then(|control| control.path)
            })
            .or_else(|| {
                self.instances
                    .config_dir()
                    .map(|dir| dir.join(format!("{instance_id}.toml")))
            })
    }

    pub(super) fn ensure_config_identity(&self, instance_id: uuid::Uuid) -> anyhow::Result<()> {
        if let (Some(active), Some(registered)) = (
            self.instances.config_control(instance_id),
            self.instances.managed_config_control(instance_id),
        ) && active.path != registered.path
        {
            anyhow::bail!(
                "running instance {instance_id} uses a different config file from the managed catalog"
            );
        }
        Ok(())
    }

    /// All remote form writes share the fresh directory identity policy,
    /// including legacy clients that cannot supply a revision. The caller
    /// already owns the process coordinator; do not acquire it again here.
    #[cfg(feature = "management")]
    async fn ambiguous_catalog_ids_locked(&self) -> anyhow::Result<HashSet<uuid::Uuid>> {
        if self.instances.config_dir().is_none() || !self.storage.supports_catalog() {
            return Ok(HashSet::new());
        }
        let snapshot = self.refresh_local_configs_locked().await?;
        let mut seen = HashSet::new();
        let mut ambiguous = HashSet::new();
        for entry in snapshot.entries {
            if let Some(instance_id) = entry.inst_id.map(uuid::Uuid::from)
                && (!seen.insert(instance_id) || entry.status == "identity_conflict")
            {
                ambiguous.insert(instance_id);
            }
        }
        Ok(ambiguous)
    }

    #[cfg(feature = "management")]
    pub(super) async fn ensure_catalog_identity_locked(
        &self,
        instance_id: uuid::Uuid,
    ) -> anyhow::Result<()> {
        if self
            .ambiguous_catalog_ids_locked()
            .await?
            .contains(&instance_id)
        {
            anyhow::bail!("configuration identity is ambiguous");
        }
        Ok(())
    }

    pub(super) async fn load_saved_config(
        &self,
        instance_id: uuid::Uuid,
    ) -> anyhow::Result<Option<(TomlConfig, ConfigFileControl)>> {
        self.ensure_config_identity(instance_id)?;
        let Some(path) = self.saved_config_path(instance_id) else {
            return Ok(None);
        };
        let Some((config, mut control)) = self
            .storage
            .load_config(&path, self.instances.config_dir().map(PathBuf::as_path))
            .await?
        else {
            return Ok(None);
        };
        if config.get_id() != instance_id {
            anyhow::bail!(
                "config file {} does not contain instance {instance_id}",
                path.display()
            );
        }
        if let Some(active) = self.instances.config_control(instance_id) {
            control.permission = ConfigFilePermission::from(
                u8::from(control.permission) | u8::from(active.permission),
            );
        }
        Ok(Some((config, control)))
    }

    /// A lifecycle operation changes enabled/runtime state itself. Fence the
    /// captured file separately after host hooks, before starting or deleting.
    /// No expected revision keeps the established non-CAS host flow.
    #[cfg(feature = "management")]
    async fn verify_lifecycle_file_locked(
        &self,
        path: &Path,
        expected: &easytier_proto::api::manage::PersistedConfigEntry,
    ) -> anyhow::Result<()> {
        let contents = self
            .storage
            .read(path)
            .await
            .map_err(|_| LocalConfigMutationError::RevisionConflict)?;
        let Some(contents) = contents else {
            if expected.persisted_raw_hash.is_empty() {
                return Ok(());
            }
            return Err(LocalConfigMutationError::RevisionConflict.into());
        };
        if local_catalog::raw_hash(&contents) != expected.persisted_raw_hash {
            return Err(LocalConfigMutationError::RevisionConflict.into());
        }
        let directory = self
            .instances
            .config_dir()
            .map(PathBuf::as_path)
            .unwrap_or(Path::new(""));
        let control = self
            .storage
            .inspect_raw_config(path, &contents, directory)
            .await
            .map_err(|_| LocalConfigMutationError::RevisionConflict)?;
        if u32::from(u8::from(control.permission)) != expected.config_permission {
            return Err(LocalConfigMutationError::RevisionConflict.into());
        }
        Ok(())
    }

    async fn stop_instances_locked(
        &self,
        ids: &[uuid::Uuid],
        require_known_instances: bool,
        check_catalog_identity: bool,
    ) -> anyhow::Result<()> {
        // Validate every recoverable config before stopping any existing
        // network. Instances without persistent configuration (e.g. started
        // without a config directory) can still be stopped; they simply
        // cannot be re-enabled from disk afterwards. Start plans additionally
        // reject ids that refer to no running instance and no config at all.
        let mut controls = Vec::new();
        for id in ids {
            match self.load_saved_config(*id).await? {
                Some((_, control)) => {
                    if control.path.as_deref().and_then(Path::parent)
                        != self.instances.config_dir().map(PathBuf::as_path)
                    {
                        anyhow::bail!("instance {id} is not managed by the config directory");
                    }
                    controls.push((*id, control));
                }
                None => {
                    if require_known_instances && self.instances.instance(*id).is_none() {
                        anyhow::bail!("instance {id} has no recoverable persistent configuration");
                    }
                }
            }
        }
        #[cfg(feature = "management")]
        if check_catalog_identity {
            let ambiguous = self.ambiguous_catalog_ids_locked().await?;
            if ids.iter().any(|id| ambiguous.contains(id)) {
                anyhow::bail!("configuration identity is ambiguous");
            }
        }
        #[cfg(not(feature = "management"))]
        let _ = check_catalog_identity;
        for (id, control) in controls {
            self.instances.register_managed_config(id, control)?;
        }
        let stopped = ids
            .iter()
            .copied()
            .filter(|id| self.instances.instance(*id).is_some())
            .collect::<Vec<_>>();
        self.state_store.set_enabled_batch(ids, false)?;
        self.instances
            .delete_network_instances(stopped.clone())
            .await?;
        if !ids.is_empty() {
            self.hooks
                .post_stop_network_instances(ids)
                .await
                .map_err(|error| anyhow::anyhow!("post-stop hook failed: {error}"))?;
        }
        Ok(())
    }

    pub(crate) async fn prepare_start_locked(&self, config: &TomlConfig) -> anyhow::Result<()> {
        let active = self
            .instances
            .instances()
            .into_iter()
            .filter_map(|instance| {
                instance.toml_config().map(|config| ActiveInstanceForStart {
                    instance_id: instance.instance_id(),
                    no_tun: config.get_flags().no_tun,
                    source: config.get_network_config_source(),
                })
            })
            .collect::<Vec<_>>();
        let ids = self
            .hooks
            .prepare_start(config, &active)
            .await
            .map_err(|error| anyhow::anyhow!("start preparation failed: {error}"))?;
        let ids = ids
            .into_iter()
            .filter(|id| *id != config.get_id())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let instance_name = config.get_inst_name();
        if self.instances.instances().iter().any(|instance| {
            instance.instance_id() != config.get_id()
                && !ids.contains(&instance.instance_id())
                && instance.instance_name() == instance_name
        }) {
            anyhow::bail!(
                "instance name {instance_name} is already used by a running instance; change the network name or stop that instance first"
            );
        }
        self.stop_instances_locked(&ids, true, false).await
    }

    pub async fn delete_network_instances_with_revision(
        &self,
        requested: Vec<uuid::Uuid>,
        expected_revision: Option<String>,
    ) -> anyhow::Result<InstanceMutationResult> {
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        #[cfg(feature = "management")]
        let expected_file = if let Some(expected) = expected_revision.as_deref() {
            if requested.len() != 1 {
                anyhow::bail!("revision requires one instance");
            }
            self.capture_local_revision_locked(requested[0], Some(expected))
                .await?
        } else {
            None
        };
        #[cfg(feature = "management")]
        let ambiguous = self.ambiguous_catalog_ids_locked().await?;
        let requested = requested.into_iter().collect::<HashSet<_>>();
        let mut removed = Vec::new();
        for instance_id in &requested {
            #[cfg(feature = "management")]
            if ambiguous.contains(instance_id) {
                continue;
            }
            let active = self.instances.config_control(*instance_id);
            if self.instances.instance(*instance_id).is_some() && active.is_none() {
                continue;
            }
            if active
                .as_ref()
                .is_some_and(|control| control.is_read_only() || !control.is_deletable())
            {
                continue;
            }
            let loaded = match self.load_saved_config(*instance_id).await {
                Ok(loaded) => loaded,
                Err(error) => {
                    tracing::warn!(%error, %instance_id, "failed to inspect config for deletion");
                    continue;
                }
            };
            if loaded
                .as_ref()
                .is_some_and(|(_, control)| control.is_read_only() || !control.is_deletable())
            {
                continue;
            }
            let path = loaded
                .as_ref()
                .and_then(|(_, control)| control.path.clone())
                .or_else(|| active.as_ref().and_then(|control| control.path.clone()));
            #[cfg(feature = "management")]
            if let Err(error) = self.ensure_catalog_identity_locked(*instance_id).await {
                tracing::warn!(%error, %instance_id, "preserving instance with uncertain identity before deletion");
                continue;
            }
            // Fence before any irreversible action: an external edit between
            // the revision capture at the start of this call and the stop
            // must reject the deletion while the instance is still running.
            // The checks run again after the stop, because the post-stop
            // hooks are allowed to rewrite or alias the file.
            if let Some(path) = path.as_ref() {
                #[cfg(feature = "management")]
                if let Some(expected) = &expected_file {
                    self.verify_lifecycle_file_locked(path, expected).await?;
                }
                #[cfg(feature = "management")]
                if let Err(error) = self.ensure_catalog_identity_locked(*instance_id).await {
                    tracing::warn!(%error, %instance_id, "preserving config with uncertain identity during deletion");
                    continue;
                }
                let current = self.storage.inspect(path).await;
                if current.is_read_only() || !current.is_deletable() {
                    continue;
                }
            }
            let was_running = self.instances.instance(*instance_id).is_some();
            if was_running {
                if let Err(error) = self.state_store.set_enabled(*instance_id, false) {
                    tracing::warn!(%error, %instance_id, "failed to persist stopped state before deletion");
                    continue;
                }
                if let Err(error) = self
                    .instances
                    .delete_network_instances([*instance_id])
                    .await
                {
                    tracing::warn!(%error, %instance_id, "failed to stop instance for deletion");
                    continue;
                }
                if let Err(error) = self
                    .hooks
                    .post_stop_network_instances(&[*instance_id])
                    .await
                {
                    tracing::warn!(%error, %instance_id, "post-stop hook failed during deletion");
                    continue;
                }
            }
            if let Some(path) = path {
                // Post-stop recheck: hooks may have rewritten the file or
                // created a duplicate identity while stopping the instance.
                #[cfg(feature = "management")]
                if let Some(expected) = &expected_file {
                    self.verify_lifecycle_file_locked(&path, expected).await?;
                }
                #[cfg(feature = "management")]
                if let Err(error) = self.ensure_catalog_identity_locked(*instance_id).await {
                    tracing::warn!(%error, %instance_id, "preserving config with uncertain identity during deletion");
                    continue;
                }
                let current = self.storage.inspect(&path).await;
                if current.is_read_only() || !current.is_deletable() {
                    continue;
                }
                match self.storage.remove(&path).await {
                    Ok(()) => {}
                    Err(error) if is_not_found_error(&error) => {}
                    Err(error) => {
                        tracing::warn!(%error, path = %path.display(), "failed to remove config file");
                        continue;
                    }
                }
            }
            if let Err(error) = self.state_store.remove(instance_id) {
                tracing::warn!(%error, %instance_id, "failed to clear deleted instance state");
                continue;
            }
            self.instances.unregister_managed_config(*instance_id);
            removed.push(*instance_id);
        }
        self.notify_removed_instances(&removed).await?;
        let mut remaining = self
            .instances
            .instance_ids()
            .into_iter()
            .chain(self.state_store.disabled_instance_ids())
            .chain(requested.into_iter().filter(|id| !removed.contains(id)))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        remaining.sort();
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
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        #[cfg(feature = "management")]
        self.ensure_catalog_identity_locked(instance_id).await?;
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
        let path = self
            .saved_config_path(instance_id)
            .unwrap_or_else(|| config_dir.join(format!("{instance_id}.toml")));
        let existing = match self.load_saved_config(instance_id).await? {
            Some((_, control)) => control,
            None => self.storage.inspect(&path).await,
        };
        if existing.is_read_only() {
            anyhow::bail!(
                "config file {} is read-only, cannot save config",
                path.display()
            );
        }
        config.set_network_config_source(requested_source.or(Some(ConfigSource::Web)));
        let previous_enabled = self.state_store.enabled_state(&instance_id);
        let mut contents = config.dump().into_bytes();
        #[cfg(feature = "management")]
        if let Some(original) = self.storage.read(&path).await? {
            contents = persisted_merge::merge_legacy_form_changes(
                std::str::from_utf8(&original)
                    .map_err(|_| anyhow::anyhow!("invalid persisted configuration"))?,
                &config,
            )?
            .into_bytes();
        }
        #[cfg(feature = "management")]
        self.ensure_catalog_identity_locked(instance_id).await?;
        self.state_store.set_enabled(instance_id, false)?;
        if let Err(error) = self.storage.write(&path, &contents).await {
            self.restore_enabled_state(instance_id, previous_enabled)?;
            return Err(anyhow::anyhow!("failed to save config file: {error}"));
        }
        self.instances.register_managed_config(
            instance_id,
            ConfigFileControl::new(Some(path), existing.permission),
        )?;
        Ok(instance_id)
    }

    /// Enables or disables a locally managed instance. Disabling stops the
    /// instance but keeps its config file; enabling restores it from the file.
    pub async fn set_network_instance_enabled(
        &self,
        instance_id: uuid::Uuid,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.set_network_instance_enabled_with_revision(instance_id, enabled, None)
            .await
    }

    pub async fn set_network_instance_enabled_with_revision(
        &self,
        instance_id: uuid::Uuid,
        enabled: bool,
        expected_revision: Option<String>,
    ) -> anyhow::Result<()> {
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        #[cfg(feature = "management")]
        let expected_file = self
            .capture_local_revision_locked(instance_id, expected_revision.as_deref())
            .await?;
        #[cfg(feature = "management")]
        self.ensure_catalog_identity_locked(instance_id).await?;
        self.ensure_config_identity(instance_id)?;
        if enabled {
            if self.instances.instance(instance_id).is_some() {
                self.state_store.set_enabled(instance_id, true)?;
                return Ok(());
            }
            if self.instances.config_dir().is_none() {
                anyhow::bail!("config dir is not configured, cannot restore instance");
            }
            let (config, control) =
                self.load_saved_config(instance_id).await?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "config file for instance {instance_id} not found, cannot enable instance"
                    )
                })?;
            if control.path.as_deref().and_then(Path::parent)
                != self.instances.config_dir().map(PathBuf::as_path)
            {
                anyhow::bail!("instance {instance_id} is not managed by the config directory");
            }
            self.state_store.set_enabled(instance_id, false)?;
            self.prepare_start_locked(&config).await?;
            self.hooks
                .pre_run_network_instance(&config)
                .await
                .map_err(|error| anyhow::anyhow!("pre-run hook failed: {error}"))?;
            #[cfg(feature = "management")]
            if let Some(expected) = &expected_file {
                let path = control
                    .path
                    .as_deref()
                    .ok_or(LocalConfigMutationError::RevisionConflict)?;
                self.verify_lifecycle_file_locked(path, expected).await?;
            }
            #[cfg(feature = "management")]
            self.ensure_catalog_identity_locked(instance_id).await?;
            self.instances
                .register_managed_config(instance_id, control.clone())?;
            self.instances.run_network_instance(config, control)?;
            if let Err(error) = self.hooks.post_run_network_instance(&instance_id).await {
                self.instances
                    .delete_network_instances([instance_id])
                    .await?;
                let _ = self.hooks.post_stop_network_instances(&[instance_id]).await;
                anyhow::bail!("post-run hook failed: {error}");
            }
        } else {
            return self
                .stop_instances_locked(&[instance_id], false, true)
                .await;
        }
        if let Err(error) = self.state_store.set_enabled(instance_id, enabled) {
            self.instances
                .delete_network_instances([instance_id])
                .await?;
            let _ = self.hooks.post_stop_network_instances(&[instance_id]).await;
            return Err(error);
        }
        Ok(())
    }

    /// Removes the config files and state entries for the given instances.
    /// Running instances are left untouched.
    pub async fn remove_network_instance_configs(
        &self,
        requested: &[uuid::Uuid],
    ) -> anyhow::Result<Vec<uuid::Uuid>> {
        self.remove_network_instance_configs_with_revision(requested, None)
            .await
    }

    pub async fn remove_network_instance_configs_with_revision(
        &self,
        requested: &[uuid::Uuid],
        expected_revision: Option<String>,
    ) -> anyhow::Result<Vec<uuid::Uuid>> {
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        #[cfg(feature = "management")]
        let expected_file = if let Some(expected) = expected_revision.as_deref() {
            if requested.len() != 1 {
                anyhow::bail!("revision requires one instance");
            }
            self.capture_local_revision_locked(requested[0], Some(expected))
                .await?
        } else {
            None
        };
        #[cfg(feature = "management")]
        let ambiguous = self.ambiguous_catalog_ids_locked().await?;
        let mut removed = Vec::new();
        for instance_id in requested {
            #[cfg(feature = "management")]
            if ambiguous.contains(instance_id) {
                continue;
            }
            if self.instances.config_dir().is_none() {
                continue;
            }
            if self
                .instances
                .config_control(*instance_id)
                .is_some_and(|control| control.is_read_only() || !control.is_deletable())
            {
                continue;
            }
            let loaded = match self.load_saved_config(*instance_id).await {
                Ok(loaded) => loaded,
                Err(error) => {
                    tracing::warn!(%error, %instance_id, "failed to inspect config for removal");
                    continue;
                }
            };
            if let Some((_, control)) = loaded {
                if control.is_read_only() || !control.is_deletable() {
                    continue;
                }
                let path = control.path.expect("loaded config file has a path");
                #[cfg(feature = "management")]
                if let Some(expected) = &expected_file {
                    self.verify_lifecycle_file_locked(&path, expected).await?;
                }
                #[cfg(feature = "management")]
                if let Err(error) = self.ensure_catalog_identity_locked(*instance_id).await {
                    tracing::warn!(%error, %instance_id, "preserving config with uncertain identity during removal");
                    continue;
                }
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
            if let Err(error) = self.state_store.remove(instance_id) {
                tracing::warn!(%error, %instance_id, "failed to clear removed config state");
                continue;
            }
            self.instances.unregister_managed_config(*instance_id);
            removed.push(*instance_id);
        }
        Ok(removed)
    }

    /// Reads a config from disk, falling back to the running instance config.
    pub async fn get_network_instance_config_from_file(
        &self,
        instance_id: uuid::Uuid,
    ) -> anyhow::Result<Option<(NetworkConfig, ConfigSource)>> {
        let _mutation = self.lock_mutation().await?;
        #[cfg(feature = "management")]
        let _refresh = self
            .instances
            .local_config_catalog()
            .refresh_after_mutation();
        #[cfg(feature = "management")]
        if self.storage.supports_catalog() && self.instances.config_dir().is_some() {
            let snapshot = self.refresh_local_configs_locked().await?;
            let entries = snapshot
                .entries
                .into_iter()
                .filter(|entry| entry.inst_id.map(uuid::Uuid::from) == Some(instance_id))
                .collect::<Vec<_>>();
            if entries.is_empty() {
                return Ok(None);
            }
            if entries.len() != 1 || entries[0].status != "ready" {
                return Err(anyhow::Error::from(LocalConfigMutationError::Protected)
                    .context("configuration is protected or unavailable"));
            }
            let entry = entries.into_iter().next().unwrap();
            return Ok(entry.config.map(|config| {
                (
                    config,
                    config_source_from_rpc(entry.source).unwrap_or(ConfigSource::User),
                )
            }));
        }
        #[cfg(feature = "management")]
        if self.storage.supports_catalog() {
            let original = crate::management::instance_rpc::persisted::read_original_config(
                &self.instances,
                self.storage.as_ref(),
                instance_id,
            )
            .await?;
            return Ok(original.map(|(config, _)| {
                let source = self
                    .instances
                    .config_source(instance_id)
                    .unwrap_or(config.get_network_config_source());
                (network_config_from_toml(&config), source)
            }));
        }
        if self
            .instances
            .managed_config_control(instance_id)
            .is_some_and(|control| control.is_read_only())
        {
            return Err(
                anyhow::Error::from(LocalConfigMutationError::ReadOnly).context(format!(
                    "configuration for instance {instance_id} is read-only"
                )),
            );
        }
        let Some((config, control)) = self.load_saved_config(instance_id).await? else {
            return Ok(None);
        };
        if control.is_read_only() {
            return Err(
                anyhow::Error::from(LocalConfigMutationError::ReadOnly).context(format!(
                    "configuration for instance {instance_id} is read-only"
                )),
            );
        }
        let source = config.get_network_config_source();
        Ok(Some((network_config_from_toml(&config), source)))
    }
}
