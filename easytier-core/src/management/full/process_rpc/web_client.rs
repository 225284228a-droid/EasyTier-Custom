use super::*;

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
                .set_network_instance_enabled_with_revision(
                    instance_id,
                    request.enabled,
                    request.expected_revision,
                )
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
        let removed = tokio::spawn(async move {
            management
                .remove_network_instance_configs_with_revision(
                    &to_remove,
                    request.expected_revision,
                )
                .await
        })
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
