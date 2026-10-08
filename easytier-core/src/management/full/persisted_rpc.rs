//! Online, node-authoritative configuration operations with compare-and-swap.

use easytier_proto::{
    api::manage::{
        ObserveConfigsRequest, ObserveConfigsResponse, PatchPersistedConfigRequest,
        PatchPersistedConfigResponse, PersistedConfigApplyMode, PersistedConfigMutationStatus,
        PersistedConfigService,
    },
    rpc_types::{self, controller::BaseController},
};

use super::{
    ProcessManagement, ProcessManagementRpc,
    local_catalog::{raw_hash, refresh_configs},
    persisted_merge::{merge_persisted_config, unsupported_capability},
    process_rpc::InstanceRunPersistence,
};
use crate::{
    config::toml::{ConfigLoader as _, TomlConfig},
    instance::{CoreInstance, CoreInstanceHost, manager::InstanceFactory},
};

fn response(
    snapshot: ObserveConfigsResponse,
    id: uuid::Uuid,
    status: PersistedConfigMutationStatus,
    message: Option<&str>,
) -> PatchPersistedConfigResponse {
    PatchPersistedConfigResponse {
        status: status as i32,
        message: message.map(str::to_owned),
        entry: snapshot
            .entries
            .into_iter()
            .find(|entry| entry.inst_id.map(uuid::Uuid::from) == Some(id)),
        catalog_epoch: snapshot.catalog_epoch,
        catalog_generation: snapshot.catalog_generation,
    }
}

impl<F, H> ProcessManagement<F>
where
    F: InstanceFactory<Instance = CoreInstance<H>, CreateContext = ()>,
    F::Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
    H: CoreInstanceHost,
{
    pub async fn observe_persisted_configs(&self) -> anyhow::Result<ObserveConfigsResponse> {
        let _coordinator = self.mutation_lock.lock().await;
        self.refresh_local_configs_locked().await
    }

    pub(super) async fn refresh_local_configs_locked(
        &self,
    ) -> anyhow::Result<ObserveConfigsResponse> {
        refresh_configs(&self.instances, self.storage.as_ref(), &self.state_store).await
    }

    /// Single-instance lifecycle CAS. No expected revision preserves the
    /// established legacy RPC contract; new clients always supply one.
    pub(super) async fn verify_local_revision_locked(
        &self,
        instance_id: uuid::Uuid,
        expected: Option<&str>,
    ) -> anyhow::Result<()> {
        self.capture_local_revision_locked(instance_id, expected)
            .await
            .map(|_| ())
    }

    pub(super) async fn capture_local_revision_locked(
        &self,
        instance_id: uuid::Uuid,
        expected: Option<&str>,
    ) -> anyhow::Result<Option<easytier_proto::api::manage::PersistedConfigEntry>> {
        let Some(expected) = expected else {
            return Ok(None);
        };
        let snapshot = self.refresh_local_configs_locked().await?;
        let entries = snapshot
            .entries
            .iter()
            .filter(|entry| entry.inst_id.map(uuid::Uuid::from) == Some(instance_id))
            .collect::<Vec<_>>();
        if entries.len() != 1 || entries[0].revision != expected {
            anyhow::bail!("local_config_revision_conflict");
        }
        Ok(Some(entries[0].clone()))
    }

    /// Caller owns this complete task, so request cancellation cannot release
    /// the coordinator while a blocking file commit or apply is still running.
    pub async fn patch_persisted_config(
        &self,
        request: PatchPersistedConfigRequest,
    ) -> anyhow::Result<PatchPersistedConfigResponse> {
        let id = request
            .inst_id
            .ok_or_else(|| anyhow::anyhow!("instance id is required"))?
            .into();
        let _coordinator = self.mutation_lock.lock().await;
        if self.instances.config_dir().is_none() || !self.storage.supports_catalog() {
            return Ok(PatchPersistedConfigResponse {
                status: PersistedConfigMutationStatus::Unsupported as i32,
                message: Some("persisted config observation is unavailable".into()),
                ..Default::default()
            });
        }
        let snapshot = self.refresh_local_configs_locked().await?;
        let existing = snapshot
            .entries
            .iter()
            .filter(|entry| entry.inst_id.map(uuid::Uuid::from) == Some(id))
            .cloned()
            .collect::<Vec<_>>();
        if existing.len() > 1 {
            return Ok(response(
                snapshot,
                id,
                PersistedConfigMutationStatus::Protected,
                Some("configuration identity is ambiguous"),
            ));
        }
        let existing = existing.into_iter().next();
        if let Some(entry) = &existing {
            if entry.revision != request.expected_revision {
                return Ok(response(
                    snapshot,
                    id,
                    PersistedConfigMutationStatus::Conflict,
                    Some("local_config_revision_conflict"),
                ));
            }
            let status = match entry.status.as_str() {
                "ready" => None,
                "protected" | "identity_conflict" => Some(PersistedConfigMutationStatus::Protected),
                "missing" => Some(PersistedConfigMutationStatus::NotFound),
                _ => Some(PersistedConfigMutationStatus::Invalid),
            };
            if let Some(status) = status {
                return Ok(response(
                    snapshot,
                    id,
                    status,
                    Some("configuration is not editable"),
                ));
            }
        } else if !request.expected_revision.is_empty() {
            return Ok(response(
                snapshot,
                id,
                PersistedConfigMutationStatus::NotFound,
                Some("configuration does not exist"),
            ));
        }
        let Some(values) = request.config.as_ref() else {
            return Ok(response(
                snapshot,
                id,
                PersistedConfigMutationStatus::Invalid,
                Some("config is required"),
            ));
        };
        if values
            .instance_id
            .as_ref()
            .is_some_and(|value| value != &id.to_string())
        {
            return Ok(response(
                snapshot,
                id,
                PersistedConfigMutationStatus::Invalid,
                Some("instance identity cannot be changed"),
            ));
        }
        let Ok(mode) = PersistedConfigApplyMode::try_from(request.apply_mode) else {
            return Ok(response(
                snapshot,
                id,
                PersistedConfigMutationStatus::Invalid,
                Some("invalid apply mode"),
            ));
        };
        let apply_only = request.field_mask.is_empty();
        if apply_only
            && (mode != PersistedConfigApplyMode::SaveAndApply
                || existing.is_none()
                || request.expected_revision.is_empty()
                || values != &Default::default())
        {
            return Ok(response(
                snapshot,
                id,
                PersistedConfigMutationStatus::Invalid,
                Some(
                    "applying persisted configuration requires an existing revision and no edited values",
                ),
            ));
        }
        let capabilities = self.instances.management_capabilities();
        if unsupported_capability(values, &request.field_mask, &capabilities).is_some() {
            return Ok(response(
                snapshot,
                id,
                PersistedConfigMutationStatus::Unsupported,
                Some("field is not supported by this node"),
            ));
        }
        let directory = self.instances.config_dir().unwrap();
        let path = self
            .instances
            .managed_config_control(id)
            .and_then(|control| control.path)
            .unwrap_or_else(|| directory.join(format!("{id}.toml")));
        let original = match &existing {
            Some(entry) => entry
                .persisted_toml
                .clone()
                .ok_or_else(|| anyhow::anyhow!("persisted configuration is unavailable"))?,
            None => {
                // Empty revision can create a new identity, but can never
                // overwrite an invalid, unidentified or foreign file.
                if self.storage.read(&path).await?.is_some() {
                    return Ok(response(
                        snapshot,
                        id,
                        PersistedConfigMutationStatus::Conflict,
                        Some("target config file already exists"),
                    ));
                }
                format!(
                    "instance_id = '{id}'\n[network_identity]\nnetwork_name = ''\nnetwork_secret = ''\n"
                )
            }
        };
        let merged = match if apply_only {
            Ok(original)
        } else {
            merge_persisted_config(&original, values, &request.field_mask)
        } {
            Ok(merged) => merged,
            Err(_) => {
                return Ok(response(
                    snapshot,
                    id,
                    PersistedConfigMutationStatus::Invalid,
                    Some("invalid field mask or configuration values"),
                ));
            }
        };
        let config = TomlConfig::new_from_str(&merged)
            .map_err(|_| anyhow::anyhow!("invalid resulting configuration"))?;
        if config.get_id() != id {
            return Ok(response(
                snapshot,
                id,
                PersistedConfigMutationStatus::Invalid,
                Some("instance identity cannot be changed"),
            ));
        }
        // Detect manual edits that happened while the typed candidate was
        // validated. Uncooperative external editors do not share this lock.
        let current = self.storage.read(&path).await?;
        let matches = match (&existing, &current) {
            (Some(entry), Some(contents)) => raw_hash(contents) == entry.persisted_raw_hash,
            (None, None) => true,
            _ => false,
        };
        if !matches {
            return Ok(response(
                self.refresh_local_configs_locked().await?,
                id,
                PersistedConfigMutationStatus::Conflict,
                Some("local_config_revision_conflict"),
            ));
        }
        let control = self
            .storage
            .inspect_raw_config(
                &path,
                current.as_deref().unwrap_or(merged.as_bytes()),
                directory,
            )
            .await?;
        if control.is_read_only() {
            return Ok(response(
                self.refresh_local_configs_locked().await?,
                id,
                PersistedConfigMutationStatus::Protected,
                Some("configuration is read-only"),
            ));
        }
        if let Some(entry) = &existing {
            let checked = self.refresh_local_configs_locked().await?;
            if !checked.entries.iter().any(|current| {
                current.inst_id.map(uuid::Uuid::from) == Some(id)
                    && current.revision == entry.revision
            }) {
                return Ok(response(
                    checked,
                    id,
                    PersistedConfigMutationStatus::Conflict,
                    Some("local_config_revision_conflict"),
                ));
            }
        }
        let apply =
            mode == PersistedConfigApplyMode::SaveAndApply && self.instances.instance(id).is_some();
        let outcome = if apply {
            self.run_network_instance_locked_with_contents(
                config,
                id,
                true,
                None,
                InstanceRunPersistence {
                    contents: Some(merged.as_bytes()),
                    apply_only,
                    expected_revision: Some(&request.expected_revision),
                },
            )
            .await
            .map(|_| ())
        } else if apply_only {
            // Applying saved configuration never enables a stopped instance or
            // rewrites its file. The checks above still fence the observation.
            Ok(())
        } else {
            let previous_enabled = self.state_store.enabled_state(&id);
            if existing.is_none() {
                self.state_store.set_enabled(id, false)?;
            }
            if let Err(error) = self.storage.write(&path, merged.as_bytes()).await {
                if existing.is_none() {
                    self.restore_enabled_state(id, previous_enabled)?;
                }
                return Err(error);
            }
            self.instances.register_managed_config(id, control)?;
            Ok(())
        };
        let refreshed = self.refresh_local_configs_locked().await?;
        Ok(response(
            refreshed,
            id,
            if outcome
                .as_ref()
                .is_err_and(|error| error.to_string() == "local_config_revision_conflict")
            {
                PersistedConfigMutationStatus::Conflict
            } else if outcome.is_ok() {
                PersistedConfigMutationStatus::Success
            } else {
                PersistedConfigMutationStatus::ApplyFailed
            },
            if outcome
                .as_ref()
                .is_err_and(|error| error.to_string() == "local_config_revision_conflict")
            {
                Some("local_config_revision_conflict")
            } else if outcome.is_ok() {
                None
            } else {
                Some("configuration could not be applied; observe its persisted and active state")
            },
        ))
    }
}

#[async_trait::async_trait]
impl<F, H> PersistedConfigService for ProcessManagementRpc<F>
where
    F: InstanceFactory<Instance = CoreInstance<H>, CreateContext = ()>,
    F::Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
    H: CoreInstanceHost,
{
    type Controller = BaseController;

    async fn observe_configs(
        &self,
        _: BaseController,
        _: ObserveConfigsRequest,
    ) -> rpc_types::error::Result<ObserveConfigsResponse> {
        self.management
            .observe_persisted_configs()
            .await
            .map_err(Into::into)
    }

    async fn patch_config(
        &self,
        _: BaseController,
        request: PatchPersistedConfigRequest,
    ) -> rpc_types::error::Result<PatchPersistedConfigResponse> {
        let management = self.management.clone();
        tokio::spawn(async move { management.patch_persisted_config(request).await })
            .await
            .map_err(|_| anyhow::anyhow!("persisted config operation failed"))?
            .map_err(Into::into)
    }
}
