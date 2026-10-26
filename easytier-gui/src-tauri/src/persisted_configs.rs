//! The GUI edits network configurations only through the connected Core's
//! config directory. Every write carries the revision it was based on.
use std::time::Duration;

use easytier::proto::{
    api::manage::{
        DeleteNetworkInstanceRequest, ObserveConfigsRequest, ObserveConfigsResponse,
        PatchPersistedConfigRequest, PatchPersistedConfigResponse, PersistedConfigService,
        SetNetworkInstanceEnabledRequest, WebClientService,
    },
    rpc_types::controller::BaseController,
};

use super::{CLIENT_MANAGER, RwLockReadGuard};

#[tauri::command]
pub(crate) async fn observe_local_configs() -> Result<ObserveConfigsResponse, String> {
    let manager = get_client_manager!()?;
    tokio::time::timeout(
        Duration::from_secs(10),
        manager
            .persisted_configs()
            .observe_configs(BaseController::default(), ObserveConfigsRequest {}),
    )
    .await
    .map_err(|_| "local_config_observation_timeout".to_owned())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn patch_local_config(
    request: PatchPersistedConfigRequest,
) -> Result<PatchPersistedConfigResponse, String> {
    let manager = get_client_manager!()?;
    let _mutation = manager.config_mutation.lock().await;
    // The caller observes again after an unknown result. Retrying a timed-out
    // save can race a write that is still completing inside Core.
    tokio::time::timeout(
        Duration::from_secs(10),
        manager
            .persisted_configs()
            .patch_config(BaseController::default(), request),
    )
    .await
    .map_err(|_| "local_config_outcome_unknown".to_owned())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn update_network_config_state(
    instance_id: String,
    disabled: bool,
    expected_revision: String,
) -> Result<(), String> {
    lifecycle(instance_id, expected_revision, Some(!disabled)).await
}

#[tauri::command]
pub(crate) async fn remove_network_instance(
    instance_id: String,
    expected_revision: String,
) -> Result<(), String> {
    lifecycle(instance_id, expected_revision, None).await
}

async fn lifecycle(
    instance_id: String,
    expected_revision: String,
    enabled: Option<bool>,
) -> Result<(), String> {
    let id: uuid::Uuid = instance_id
        .parse()
        .map_err(|error: uuid::Error| error.to_string())?;
    let manager = get_client_manager!()?;
    let _mutation = manager.config_mutation.lock().await;
    let client = manager.web_client();
    tokio::time::timeout(Duration::from_secs(10), async {
        match enabled {
            Some(enabled) => client
                .set_network_instance_enabled(
                    BaseController::default(),
                    SetNetworkInstanceEnabledRequest {
                        inst_id: Some(id.into()),
                        enabled,
                        expected_revision: Some(expected_revision),
                    },
                )
                .await
                .map(|_| ()),
            None => client
                .delete_network_instance(
                    BaseController::default(),
                    DeleteNetworkInstanceRequest {
                        inst_ids: vec![id.into()],
                        expected_revision: Some(expected_revision),
                    },
                )
                .await
                .and_then(|response| {
                    if response.remain_inst_ids.contains(&id.into()) {
                        Err(anyhow::anyhow!("configuration could not be removed").into())
                    } else {
                        Ok(())
                    }
                }),
        }
    })
    .await
    .map_err(|_| "local_config_outcome_unknown".to_owned())?
    .map_err(|error| error.to_string())
}
