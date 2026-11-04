//! Bodies of the custom `WebClientService` config-file RPCs. The trait impl in
//! `process_rpc.rs` only forwards to them so the official impl stays intact.

use super::*;

impl<F, H> ProcessManagementRpc<F>
where
    F: InstanceFactory<Instance = CoreInstance<H>, CreateContext = ()>,
    F::Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
    H: CoreInstanceHost,
{
    pub(super) async fn save_persisted_config(
        &self,
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

    pub(super) async fn set_persisted_config_enabled(
        &self,
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

    pub(super) async fn remove_persisted_configs(
        &self,
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
