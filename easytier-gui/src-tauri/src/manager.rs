use super::*;
use async_trait::async_trait;
#[cfg(any(test, target_os = "android"))]
use easytier::common::config::ConfigSource;
use easytier::proto::api::logger::{LoggerRpc, LoggerRpcClientFactory, SetLoggerConfigRequest};
use easytier::proto::api::manage::{
    CollectNetworkInfoRequest, ListNetworkInstanceMetaRequest, ListNetworkInstanceRequest,
    NetworkMeta, PersistedConfigService, PersistedConfigServiceClientFactory,
    ValidateConfigRequest,
};
use easytier::proto::rpc::bidirect::BidirectRpcManager;
use easytier::web_client::WebClientHooks;
use easytier_core::management::ActiveInstanceForStart;

/// The settings Android needs to attach a network to its VPN service.
#[derive(Clone, serde::Serialize)]
pub(super) struct MobileNetworkProfile {
    no_tun: bool,
    dhcp: bool,
    dhcpv6: bool,
    virtual_ipv4: Option<String>,
}

impl MobileNetworkProfile {
    pub(super) fn from_config(config: &TomlConfigLoader) -> Result<Self, String> {
        let config = NetworkConfig::new_from_config(config).map_err(|e| e.to_string())?;
        Ok(Self {
            no_tun: config.no_tun.unwrap_or_default(),
            dhcp: config.dhcp.unwrap_or_default(),
            dhcpv6: config.dhcpv6.unwrap_or_default(),
            virtual_ipv4: config.virtual_ipv4,
        })
    }
}

/// Profiles of configurations that are about to start. Android prepares
/// its VPN service before the instance exists in the manager.
pub(super) static PREPARED_PROFILES: once_cell::sync::Lazy<
    dashmap::DashMap<Uuid, MobileNetworkProfile>,
> = once_cell::sync::Lazy::new(dashmap::DashMap::new);

pub(super) struct GuiHooks {
    pub(super) app: AppHandle,
    pub(super) instances: Arc<NativeInstanceManager>,
}

impl GuiHooks {
    fn notify_vpn_stop_if_no_tun(&self) -> Result<(), String> {
        let has_tun = self.instances.instance_ids().into_iter().any(|id| {
            self.instances
                .config(id)
                .is_some_and(|config| !config.get_flags().no_tun)
        });
        if !has_tun {
            self.app
                .emit("vpn_service_stop", "")
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

#[cfg(any(test, target_os = "android"))]
fn tun_instances_to_stop(
    id: Uuid,
    no_tun: bool,
    source: ConfigSource,
    active: &[ActiveInstanceForStart],
) -> Result<Vec<Uuid>, String> {
    if no_tun {
        return Ok(Vec::new());
    }
    let other_tun = active
        .iter()
        .filter(|instance| instance.instance_id != id && !instance.no_tun);
    if source == ConfigSource::Web
        && active
            .iter()
            .any(|instance| !instance.no_tun && instance.source != ConfigSource::Web)
    {
        return Err(
            "Android only supports one active TUN network; user-managed VPN remains active"
                .to_string(),
        );
    }
    Ok(other_tun.map(|instance| instance.instance_id).collect())
}

#[async_trait]
impl WebClientHooks for GuiHooks {
    async fn prepare_start(
        &self,
        cfg: &TomlConfigLoader,
        active: &[ActiveInstanceForStart],
    ) -> Result<Vec<Uuid>, String> {
        #[cfg(target_os = "android")]
        {
            tun_instances_to_stop(
                cfg.get_id(),
                cfg.get_flags().no_tun,
                cfg.get_network_config_source(),
                active,
            )
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (cfg, active);
            Ok(Vec::new())
        }
    }

    async fn pre_run_network_instance(&self, cfg: &TomlConfigLoader) -> Result<(), String> {
        PREPARED_PROFILES.insert(cfg.get_id(), MobileNetworkProfile::from_config(cfg)?);
        self.app
            .emit("pre_run_network_instance", cfg.get_id().to_string())
            .map_err(|e| e.to_string())
    }

    async fn post_run_network_instance(&self, instance_id: &uuid::Uuid) -> Result<(), String> {
        #[cfg(target_os = "android")]
        if let Some(instance) = self.instances.instance(*instance_id)
            && let Some(mut receiver) = subscribe_native_instance_event(&instance)
        {
            let app = self.app.clone();
            let id = instance_id.to_string();
            tokio::spawn(async move {
                loop {
                    let event = match receiver.recv().await {
                        Ok(
                            easytier::common::global_ctx::GlobalCtxEvent::DhcpIpv4Changed(_, _)
                            | easytier::common::global_ctx::GlobalCtxEvent::DhcpIpv4Conflicted(_)
                            | easytier::common::global_ctx::GlobalCtxEvent::DhcpIpv6Changed(_, _)
                            | easytier::common::global_ctx::GlobalCtxEvent::DhcpIpv6Conflicted(_),
                        ) => "dhcp_ip_changed",
                        Ok(
                            easytier::common::global_ctx::GlobalCtxEvent::ProxyCidrsUpdated(_, _)
                            | easytier::common::global_ctx::GlobalCtxEvent::PublicIpv6Changed(_, _)
                            | easytier::common::global_ctx::GlobalCtxEvent::PublicIpv6RoutesUpdated(
                                _,
                                _,
                            ),
                        ) => "proxy_cidrs_updated",
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            receiver = receiver.resubscribe();
                            "event_lagged"
                        }
                    };
                    let _ = app.emit(event, &id);
                }
            });
        }
        self.app
            .emit("post_run_network_instance", instance_id.to_string())
            .map_err(|e| e.to_string())
    }

    async fn post_stop_network_instances(&self, ids: &[Uuid]) -> Result<(), String> {
        for id in ids {
            PREPARED_PROFILES.remove(id);
        }
        self.notify_vpn_stop_if_no_tun()
    }

    async fn post_remove_network_instances(&self, ids: &[uuid::Uuid]) -> Result<(), String> {
        for id in ids {
            PREPARED_PROFILES.remove(id);
        }
        self.notify_vpn_stop_if_no_tun()
    }
}

#[derive(serde::Serialize)]
pub(super) struct ListNetworkInstanceIdsResponse {
    running_inst_ids: Vec<easytier::proto::common::Uuid>,
    disabled_inst_ids: Vec<easytier::proto::common::Uuid>,
    runtime_capabilities: Vec<String>,
}

#[derive(serde::Serialize)]
pub(super) struct GetNetworkMetasResponse {
    metas: std::collections::HashMap<Uuid, NetworkMeta>,
}

/// RPC connection to the Core whose config directory the GUI manages.
pub(super) struct GUIClientManager {
    pub(super) rpc_manager: BidirectRpcManager,
    pub(super) config_mutation: Mutex<()>,
}

impl GUIClientManager {
    pub async fn new(
        rpc_url: Option<String>,
        local_process_runtime: Option<Arc<CoreProcessRuntime>>,
    ) -> Result<Self, anyhow::Error> {
        let tunnel = if let Some(url) = rpc_url {
            runtime_rpc_dialer(url.parse()?).connect().await?
        } else {
            local_process_runtime
                .context("local RPC requires a core process runtime")?
                .connect_ring_tunnel(*RPC_RING_UUID.deref())?
        };
        let rpc_manager = BidirectRpcManager::new();
        rpc_manager.run_with_tunnel(tunnel);
        Ok(Self {
            rpc_manager,
            config_mutation: Mutex::new(()),
        })
    }

    pub(super) fn web_client(
        &self,
    ) -> Box<dyn WebClientService<Controller = BaseController> + Send + Sync> {
        self.rpc_manager
            .rpc_client()
            .scoped_client::<WebClientServiceClientFactory<BaseController>>(1, 1, String::new())
    }

    pub(super) fn persisted_configs(
        &self,
    ) -> Box<dyn PersistedConfigService<Controller = BaseController> + Send + Sync> {
        self.rpc_manager
            .rpc_client()
            .scoped_client::<PersistedConfigServiceClientFactory<BaseController>>(
                1,
                1,
                String::new(),
            )
    }

    pub(super) fn config_rpc(
        &self,
    ) -> Box<dyn ConfigRpc<Controller = BaseController> + Send + Sync> {
        self.rpc_manager
            .rpc_client()
            .scoped_client::<ConfigRpcClientFactory<BaseController>>(1, 1, String::new())
    }

    fn logger_rpc(&self) -> Box<dyn LoggerRpc<Controller = BaseController> + Send + Sync> {
        self.rpc_manager
            .rpc_client()
            .scoped_client::<LoggerRpcClientFactory<BaseController>>(1, 1, String::new())
    }

    pub(super) async fn set_logging_level(&self, level: String) -> Result<(), anyhow::Error> {
        self.logger_rpc()
            .set_logger_config(
                BaseController::default(),
                SetLoggerConfigRequest {
                    level: easytier_core::management::parse_log_level(&level).into(),
                },
            )
            .await?;
        Ok(())
    }

    pub(super) async fn list_network_instance_ids(
        &self,
    ) -> anyhow::Result<ListNetworkInstanceIdsResponse> {
        let response = self
            .web_client()
            .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
            .await?;
        Ok(ListNetworkInstanceIdsResponse {
            running_inst_ids: response.inst_ids,
            disabled_inst_ids: response.disabled_inst_ids,
            runtime_capabilities: response.runtime_capabilities,
        })
    }

    pub(super) async fn collect_network_info(
        &self,
        instance_id: Uuid,
    ) -> anyhow::Result<CollectNetworkInfoResponse> {
        Ok(self
            .web_client()
            .collect_network_info(
                BaseController::default(),
                CollectNetworkInfoRequest {
                    inst_ids: vec![instance_id.into()],
                },
            )
            .await?)
    }

    pub(super) async fn validate_config(
        &self,
        config: NetworkConfig,
    ) -> anyhow::Result<ValidateConfigResponse> {
        Ok(self
            .web_client()
            .validate_config(
                BaseController::default(),
                ValidateConfigRequest {
                    config: Some(config),
                },
            )
            .await?)
    }

    pub(super) async fn get_network_metas(
        &self,
        instance_ids: Vec<Uuid>,
    ) -> anyhow::Result<GetNetworkMetasResponse> {
        let response = self
            .web_client()
            .list_network_instance_meta(
                BaseController::default(),
                ListNetworkInstanceMetaRequest {
                    inst_ids: instance_ids.into_iter().map(Into::into).collect(),
                },
            )
            .await?;
        Ok(GetNetworkMetasResponse {
            metas: response
                .metas
                .into_iter()
                .filter_map(|meta| Some((meta.inst_id?.into(), meta)))
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_start_plans_are_pure_and_preserve_user_tun_ownership() {
        let id = Uuid::new_v4();
        let user = Uuid::new_v4();
        let web = Uuid::new_v4();
        let active = [
            ActiveInstanceForStart {
                instance_id: user,
                no_tun: false,
                source: ConfigSource::User,
            },
            ActiveInstanceForStart {
                instance_id: web,
                no_tun: false,
                source: ConfigSource::Web,
            },
        ];
        assert_eq!(
            tun_instances_to_stop(id, true, ConfigSource::User, &active).unwrap(),
            Vec::<Uuid>::new()
        );
        assert_eq!(
            tun_instances_to_stop(id, false, ConfigSource::User, &active).unwrap(),
            vec![user, web]
        );
        assert!(tun_instances_to_stop(id, false, ConfigSource::Web, &active).is_err());
        assert!(tun_instances_to_stop(user, false, ConfigSource::Web, &active).is_err());
        assert_eq!(
            tun_instances_to_stop(web, false, ConfigSource::Web, &active[1..]).unwrap(),
            Vec::<Uuid>::new()
        );
        assert_eq!(
            tun_instances_to_stop(id, false, ConfigSource::Web, &active[1..]).unwrap(),
            vec![web]
        );
    }
}
