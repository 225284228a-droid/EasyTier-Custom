use easytier_proto::{
    api::config::{
        ConfigRpc, GetConfigRequest, GetConfigResponse, PatchConfigRequest, PatchConfigResponse,
    },
    rpc_types::{self, controller::BaseController},
};

use crate::{
    config::{api::network_config_from_toml, toml::ConfigLoader as _},
    management::apply_config_patch,
};

use super::{ReadOnlyInstanceResolver, ResolvedInstanceManagementRpc};

#[async_trait::async_trait]
impl<R> ConfigRpc for ResolvedInstanceManagementRpc<R>
where
    R: ReadOnlyInstanceResolver,
{
    type Controller = BaseController;

    async fn patch_config(
        &self,
        _: BaseController,
        request: PatchConfigRequest,
    ) -> rpc_types::error::Result<PatchConfigResponse> {
        let rpc = self.clone();
        tokio::spawn(async move {
            let lock = rpc.resolver.mutation_lock();
            let _coordinator = match lock {
                Some(lock) => Some(lock.lock_owned().await),
                None => None,
            };
            let instance = rpc.instance(request.instance.as_ref())?;
            if rpc
                .resolver
                .config_control(instance.instance_id())
                .is_some_and(|control| control.is_read_only())
            {
                return Err(anyhow::anyhow!("configuration is protected").into());
            }
            if let Some(patch) = request.patch {
                apply_config_patch(&instance, patch, rpc.config_patch_persistence.as_deref())
                    .await?;
            }
            Ok(PatchConfigResponse::default())
        })
        .await
        .map_err(|_| anyhow::anyhow!("config patch operation failed"))?
    }

    async fn get_config(
        &self,
        _: BaseController,
        request: GetConfigRequest,
    ) -> rpc_types::error::Result<GetConfigResponse> {
        let lock = self.resolver.mutation_lock();
        let _coordinator = match lock {
            Some(lock) => Some(lock.lock_owned().await),
            None => None,
        };
        let instance = self.instance(request.instance.as_ref())?;
        if self
            .resolver
            .config_control(instance.instance_id())
            .is_some_and(|control| control.is_read_only())
        {
            return Err(anyhow::anyhow!("configuration is protected").into());
        }
        if let Some(persistence) = &self.config_patch_persistence
            && let Some((config, raw)) = persistence.read_config(instance.instance_id()).await?
        {
            return Ok(GetConfigResponse {
                config: Some(network_config_from_toml(&config)),
                toml_config: raw,
            });
        }
        if self.resolver.requires_persisted_read() {
            return Err(anyhow::anyhow!("persisted configuration is unavailable").into());
        }
        let config = instance
            .toml_config()
            .ok_or_else(|| anyhow::anyhow!("shared TOML configuration is not available"))?;
        Ok(GetConfigResponse {
            config: Some(network_config_from_toml(&config)),
            toml_config: config.dump(),
        })
    }
}
