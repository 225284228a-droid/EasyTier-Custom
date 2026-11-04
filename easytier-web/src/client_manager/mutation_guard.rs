//! Web-side overrides of the remote-client runtime mutation flow.
//!
//! Split out of `mod.rs` so the upstream file stays close to its original
//! shape; the logic itself is unchanged.

use std::collections::HashSet;

use async_trait::async_trait;
use easytier::{
    common::config::{ConfigSource, config_source_from_rpc, config_source_to_rpc},
    proto::{
        api::config::ConfigRpc,
        api::manage::{
            CollectNetworkInfoResponse, DeleteNetworkInstanceRequest,
            DeleteNetworkInstanceResponse, GetNetworkInstanceConfigRequest,
            ListNetworkInstanceRequest, RemoveNetworkInstanceConfigRequest,
            RunNetworkInstanceRequest, SaveNetworkInstanceConfigRequest,
            SetNetworkInstanceEnabledRequest, WebClientService,
        },
        rpc_types::controller::BaseController,
    },
};
use easytier_core::management::remote_client::{
    self, ListNetworkInstanceIdsJsonResp, RemoteClientError, RemoteClientManager,
    Storage as RemoteStorage,
};
use uuid::Uuid;

use super::ClientManager;
use crate::db::{UserIdInDb, entity::user_running_network_configs};

#[async_trait]
impl
    RemoteClientManager<
        (UserIdInDb, uuid::Uuid),
        user_running_network_configs::Model,
        sea_orm::DbErr,
    > for ClientManager
{
    fn get_rpc_client(
        &self,
        (user_id, machine_id): (UserIdInDb, uuid::Uuid),
    ) -> Option<Box<dyn WebClientService<Controller = BaseController> + Send>> {
        let s = self.get_session_by_machine_id(user_id, &machine_id)?;
        Some(s.scoped_rpc_client())
    }

    fn get_config_rpc_client(
        &self,
        (user_id, machine_id): (UserIdInDb, uuid::Uuid),
    ) -> Option<Box<dyn ConfigRpc<Controller = BaseController> + Send>> {
        let session = self.get_session_by_machine_id(user_id, &machine_id)?;
        Some(session.scoped_config_client())
    }

    fn get_storage(
        &self,
    ) -> &impl remote_client::Storage<
        (UserIdInDb, uuid::Uuid),
        user_running_network_configs::Model,
        sea_orm::DbErr,
    > {
        self.storage.db()
    }

    async fn runtime_mutation_guard(
        &self,
        identify: &(UserIdInDb, uuid::Uuid),
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        Some(
            self.storage
                .runtime_mutation_lock(*identify)
                .lock_owned()
                .await,
        )
    }

    async fn manages_local_configs(&self, identify: &(UserIdInDb, uuid::Uuid)) -> bool {
        self.supports_local_configs(identify).await
    }

    async fn on_network_info_collected(
        &self,
        identify: &(UserIdInDb, uuid::Uuid),
        full_snapshot: bool,
        response: &mut CollectNetworkInfoResponse,
    ) {
        if full_snapshot
            && let Some(cache) = &self.city_geo_cache
            && let Some(session) = self.get_session_by_machine_id(identify.0, &identify.1)
        {
            Self::apply_network_location_snapshot(&session, response, cache, self.geoip_db.clone())
                .await;
        }
    }

    async fn verify_network_instance_stopped(
        &self,
        _identify: &(UserIdInDb, uuid::Uuid),
        inst_id: &uuid::Uuid,
        response: &DeleteNetworkInstanceResponse,
    ) -> Result<(), RemoteClientError<sea_orm::DbErr>> {
        if response.remain_inst_ids.contains(&(*inst_id).into()) {
            return Err(RemoteClientError::Other(format!(
                "network instance {inst_id} could not be stopped"
            )));
        }
        Ok(())
    }

    async fn list_network_instance_ids_for_local_config(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
    ) -> Result<ListNetworkInstanceIdsJsonResp, RemoteClientError<sea_orm::DbErr>> {
        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        let ret = client
            .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
            .await?;
        // The node's own catalog reports running and disabled instances; the
        // web database only mirrors read-only observations for these nodes.
        Ok(ListNetworkInstanceIdsJsonResp {
            running_inst_ids: ret.inst_ids.into_iter().collect(),
            disabled_inst_ids: ret.disabled_inst_ids.into_iter().collect(),
            runtime_capabilities: ret.runtime_capabilities,
        })
    }

    async fn run_network_instance_for_local_config(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        config: easytier::proto::api::manage::NetworkConfig,
        _save: bool,
        source: ConfigSource,
    ) -> Result<(), RemoteClientError<sea_orm::DbErr>> {
        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        client
            .run_network_instance(
                BaseController::default(),
                RunNetworkInstanceRequest {
                    inst_id: None,
                    config: Some(config),
                    overwrite: true,
                    source: config_source_to_rpc(source),
                },
            )
            .await?;
        // The core persists the config file in its working directory; the web
        // console keeps nothing.
        Ok(())
    }

    async fn handle_remove_network_instances(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        inst_ids: Vec<uuid::Uuid>,
    ) -> Result<(), RemoteClientError<sea_orm::DbErr>> {
        if inst_ids.is_empty() {
            return Ok(());
        }
        let _mutation = self
            .storage
            .runtime_mutation_lock(identify)
            .lock_owned()
            .await;
        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        let local_configs = self.supports_local_configs(&identify).await;
        let complete_delete = local_configs
            && client
                .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
                .await?
                .supports_persisted_config_management
                == Some(true);
        let response = client
            .delete_network_instance(
                BaseController::default(),
                DeleteNetworkInstanceRequest {
                    inst_ids: inst_ids.iter().copied().map(Into::into).collect(),

                    ..Default::default()
                },
            )
            .await?;
        let mut remaining: HashSet<Uuid> = response
            .remain_inst_ids
            .into_iter()
            .map(Into::into)
            .filter(|id| inst_ids.contains(id))
            .collect();
        if local_configs && !complete_delete {
            // Older local-config cores only delete running instances. Never
            // pass a refused deletion on to their config-only removal RPC.
            let stopped: Vec<_> = inst_ids
                .iter()
                .filter(|id| !remaining.contains(id))
                .copied()
                .map(Into::into)
                .collect();
            if !stopped.is_empty() {
                let response = client
                    .remove_network_instance_config(
                        BaseController::default(),
                        RemoveNetworkInstanceConfigRequest {
                            inst_ids: stopped,
                            ..Default::default()
                        },
                    )
                    .await?;
                remaining.extend(response.remain_inst_ids.into_iter().map(Uuid::from));
            }
        }
        if !local_configs {
            let removed: Vec<_> = inst_ids
                .iter()
                .filter(|id| !remaining.contains(id))
                .copied()
                .collect();
            self.get_storage()
                .delete_network_configs(identify, &removed)
                .await
                .map_err(RemoteClientError::PersistentError)?;
        }
        if !remaining.is_empty() {
            return Err(RemoteClientError::Other(format!(
                "failed to delete network instances: {}",
                inst_ids
                    .iter()
                    .filter(|id| remaining.contains(id))
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(())
    }

    async fn update_network_state_for_local_config(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        inst_id: uuid::Uuid,
        disabled: bool,
    ) -> Result<(), RemoteClientError<sea_orm::DbErr>> {
        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        // Both directions go through set_network_instance_enabled. Enable
        // restores the instance from its on-disk config file, mirroring the
        // disable path and avoiding the redundant NetworkConfig<->TOML
        // round-trip + file rewrite that run_network_instance(overwrite=true)
        // would perform.
        client
            .set_network_instance_enabled(
                BaseController::default(),
                SetNetworkInstanceEnabledRequest {
                    inst_id: Some(inst_id.into()),
                    enabled: !disabled,

                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    async fn save_network_config_for_local_config(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        inst_id: uuid::Uuid,
        config: easytier::proto::api::manage::NetworkConfig,
        source: ConfigSource,
    ) -> Result<(), RemoteClientError<sea_orm::DbErr>> {
        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        client
            .save_network_instance_config(
                BaseController::default(),
                SaveNetworkInstanceConfigRequest {
                    inst_id: Some(inst_id.into()),
                    config: Some(config),
                    source: config_source_to_rpc(source),
                },
            )
            .await?;
        Ok(())
    }

    async fn get_network_config_for_local_config(
        &self,
        identify: (UserIdInDb, uuid::Uuid),
        inst_id: uuid::Uuid,
    ) -> Result<
        (easytier::proto::api::manage::NetworkConfig, ConfigSource),
        RemoteClientError<sea_orm::DbErr>,
    > {
        let client = self
            .get_rpc_client(identify)
            .ok_or(RemoteClientError::ClientNotFound)?;
        let resp = client
            .get_network_instance_config(
                BaseController::default(),
                GetNetworkInstanceConfigRequest {
                    inst_id: Some(inst_id.into()),
                },
            )
            .await?;
        let config = resp.config.ok_or_else(|| {
            RemoteClientError::NotFound(format!("No such network instance: {}", inst_id))
        })?;
        Ok((
            config,
            config_source_from_rpc(resp.source).unwrap_or(ConfigSource::User),
        ))
    }
}

impl ClientManager {
    pub(crate) async fn supports_local_configs(&self, identify: &(UserIdInDb, uuid::Uuid)) -> bool {
        let (user_id, machine_id) = *identify;
        let Some(session) = self.get_session_by_machine_id(user_id, &machine_id) else {
            // Remember legacy custom nodes too; an offline observation never
            // becomes desired state for the official publisher.
            return match self
                .storage
                .db()
                .known_local_config_mode(user_id, machine_id)
                .await
            {
                Ok(local) => local,
                Err(error) => {
                    tracing::warn!(%error, "node management mode unavailable; deferring managed publication");
                    true
                }
            };
        };
        session.data().read().await.support_local_configs()
    }
}
