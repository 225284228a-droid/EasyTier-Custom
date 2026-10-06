use axum::extract::{DefaultBodyLimit, Path};
use axum::http::StatusCode;
use axum::routing::{delete, post, put};
use axum::{Json, Router, extract::State, routing::get};
use axum_login::AuthUser;
use easytier::common::config::{
    ConfigSource as RuntimeConfigSource, NetworkConfig, config_source_from_rpc,
};
use easytier::proto::common::Void;
use easytier::proto::{api::manage::*, web::*};
use easytier_core::management::remote_client::{
    GetNetworkMetasResponse, ListNetworkInstanceIdsJsonResp, RemoteClientError, RemoteClientManager,
};
use sea_orm::DbErr;

use crate::client_manager::{ClientManager, session::Location};
use crate::db::UserIdInDb;

use super::users::AuthSession;
use super::{
    AppState, AppStateInner, Error, HttpHandleError, RpcError, convert_db_error, other_error,
};

const MAX_MANAGED_CONFIG_REQUEST_BODY_SIZE: usize = 32 * 1024 * 1024;

fn convert_rpc_error(e: RpcError) -> (StatusCode, Json<Error>) {
    let status_code = match &e {
        RpcError::ExecutionError(_) => StatusCode::BAD_REQUEST,
        RpcError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::BAD_GATEWAY,
    };
    let error = Error {
        message: format!("{:?}", e),
        code: None,
        current_config_revision: None,
    };
    (status_code, Json(error))
}

fn convert_error(e: RemoteClientError<DbErr>) -> (StatusCode, Json<Error>) {
    match e {
        RemoteClientError::PersistentError(e) => convert_db_error(e),
        RemoteClientError::RpcError(e) => convert_rpc_error(e),
        RemoteClientError::ClientNotFound => (
            StatusCode::NOT_FOUND,
            other_error("Client not found").into(),
        ),
        RemoteClientError::NotFound(msg) => (StatusCode::NOT_FOUND, other_error(msg).into()),
        RemoteClientError::Other(msg) => {
            (StatusCode::INTERNAL_SERVER_ERROR, other_error(msg).into())
        }
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ValidateConfigJsonReq {
    config: NetworkConfig,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct SaveNetworkJsonReq {
    config: NetworkConfig,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct RunNetworkJsonReq {
    config: NetworkConfig,
    save: bool,
    source: Option<i32>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct CollectNetworkInfoJsonReq {
    inst_ids: Option<Vec<uuid::Uuid>>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct UpdateNetworkStateJsonReq {
    disabled: bool,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct GetNetworkMetasJsonReq {
    instance_ids: Vec<uuid::Uuid>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct RemoveNetworkJsonReq {
    inst_ids: Vec<uuid::Uuid>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ManagedNetworkConfigJson {
    instance_id: uuid::Uuid,
    network_config: serde_json::Value,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ReconcileManagedNetworkConfigsJsonReq {
    managed_network_configs: Vec<ManagedNetworkConfigJson>,
    config_revision: Option<String>,
    expected_config_revision: Option<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct PatchManagedNetworkConfigsJsonReq {
    upserts: Vec<ManagedNetworkConfigJson>,
    delete_instance_ids: Vec<uuid::Uuid>,
    config_revision: String,
    expected_config_revision: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ListMachineItem {
    client_url: Option<url::Url>,
    /// Address reported by the connected EasyTier network, not the
    /// config-server transport endpoint (which may be a CDN/FRP proxy).
    public_ip: Option<String>,
    info: Option<HeartbeatRequest>,
    location: Option<Location>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ListMachineJsonResp {
    machines: Vec<ListMachineItem>,
}

pub struct NetworkApi;

impl NetworkApi {
    fn usable_node_ip(ip: &std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(ip) => {
                let octets = ip.octets();
                !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_private()
                    && !ip.is_link_local()
                    && !ip.is_multicast()
                    && !ip.is_broadcast()
                    && octets[0] != 0
                    && octets[0] < 240
                    && !(octets[0] == 100 && (64..128).contains(&octets[1]))
            }
            std::net::IpAddr::V6(ip) => {
                !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_unique_local()
                    && !ip.is_unicast_link_local()
                    && !ip.is_multicast()
            }
        }
    }

    fn node_public_ip(node: &MyNodeInfo) -> Option<std::net::IpAddr> {
        // STUN is observed by the running mesh, whereas a web-console
        // transport endpoint may belong to a CDN/FRP proxy.
        node.stun_info
            .as_ref()
            .and_then(|stun| {
                stun.public_ip
                    .iter()
                    .filter_map(|value| value.parse::<std::net::IpAddr>().ok())
                    .find(Self::usable_node_ip)
            })
            .or_else(|| {
                node.ips
                    .as_ref()
                    .and_then(|ips| ips.public_ipv4)
                    .map(std::net::Ipv4Addr::from)
                    .map(std::net::IpAddr::V4)
                    .filter(Self::usable_node_ip)
            })
            .or_else(|| {
                node.ips
                    .as_ref()
                    .and_then(|ips| ips.public_ipv6)
                    .map(std::net::Ipv6Addr::from)
                    .map(std::net::IpAddr::V6)
                    .filter(Self::usable_node_ip)
            })
    }

    async fn annotate_network_locations(
        client_mgr: &ClientManager,
        user_id: UserIdInDb,
        machine_id: uuid::Uuid,
        response: &mut CollectNetworkInfoResponse,
    ) {
        let Some(info) = response.info.as_mut() else {
            return;
        };

        for detail in info.map.values_mut() {
            let Some(node) = detail.my_node_info.as_ref() else {
                continue;
            };
            let Some(ip) = Self::node_public_ip(node) else {
                continue;
            };
            let Some(location) = client_mgr.lookup_ip_location(ip) else {
                continue;
            };
            client_mgr
                .cache_network_location(user_id, machine_id, ip.to_string(), location.clone())
                .await;
            detail.node_location = Some(NodeLocation {
                public_ip: ip.to_string(),
                country: location.country,
                city: location.city.unwrap_or_default(),
                region: location.region.unwrap_or_default(),
                latitude: location.latitude,
                longitude: location.longitude,
            });
        }
    }

    fn convert_managed_config_error(error: anyhow::Error) -> HttpHandleError {
        let (status, code, current_config_revision) =
            match error.downcast_ref::<crate::client_manager::ManagedConfigError>() {
                Some(crate::client_manager::ManagedConfigError::Invalid(_)) => {
                    (StatusCode::BAD_REQUEST, None, None)
                }
                Some(crate::client_manager::ManagedConfigError::RevisionConflict {
                    current,
                    ..
                }) => (
                    StatusCode::CONFLICT,
                    Some("managed_config_revision_conflict".to_string()),
                    current.clone(),
                ),
                Some(crate::client_manager::ManagedConfigError::OwnershipConflict { .. }) => (
                    StatusCode::CONFLICT,
                    Some("managed_config_ownership_conflict".to_string()),
                    None,
                ),
                None => (StatusCode::INTERNAL_SERVER_ERROR, None, None),
            };
        (
            status,
            Json(Error {
                message: error.to_string(),
                code,
                current_config_revision,
            }),
        )
    }

    fn get_user_id(auth_session: &AuthSession) -> Result<UserIdInDb, (StatusCode, Json<Error>)> {
        let Some(user_id) = auth_session.user.as_ref().map(|x| x.id()) else {
            return Err((
                StatusCode::UNAUTHORIZED,
                other_error("No user id found".to_string()).into(),
            ));
        };
        Ok(user_id)
    }

    async fn handle_validate_config(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path(machine_id): Path<uuid::Uuid>,
        Json(payload): Json<ValidateConfigJsonReq>,
    ) -> Result<Json<ValidateConfigResponse>, HttpHandleError> {
        Ok(client_mgr
            .handle_validate_config(
                (Self::get_user_id(&auth_session)?, machine_id),
                payload.config,
            )
            .await
            .map_err(convert_error)?
            .into())
    }

    async fn handle_run_network_instance(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path(machine_id): Path<uuid::Uuid>,
        Json(payload): Json<RunNetworkJsonReq>,
    ) -> Result<Json<Void>, HttpHandleError> {
        let user_id = Self::get_user_id(&auth_session)?;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        let result = client_mgr
            .handle_run_network_instance_with_source(
                (user_id, machine_id),
                payload.config,
                payload.save,
                RuntimeConfigSource::Web,
            )
            .await;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        result.map_err(convert_error)?;
        Ok(Void::default().into())
    }

    async fn handle_collect_one_network_info(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path((machine_id, inst_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    ) -> Result<Json<CollectNetworkInfoResponse>, HttpHandleError> {
        let mut response: CollectNetworkInfoResponse = client_mgr
            .handle_collect_network_info(
                (Self::get_user_id(&auth_session)?, machine_id),
                Some(vec![inst_id]),
            )
            .await
            .map_err(convert_error)?
            .into();
        Self::annotate_network_locations(
            &client_mgr,
            Self::get_user_id(&auth_session)?,
            machine_id,
            &mut response,
        )
        .await;
        Ok(Json(response))
    }

    async fn handle_collect_network_info(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path(machine_id): Path<uuid::Uuid>,
        Json(payload): Json<CollectNetworkInfoJsonReq>,
    ) -> Result<Json<CollectNetworkInfoResponse>, HttpHandleError> {
        let mut response: CollectNetworkInfoResponse = client_mgr
            .handle_collect_network_info(
                (Self::get_user_id(&auth_session)?, machine_id),
                payload.inst_ids,
            )
            .await
            .map_err(convert_error)?
            .into();
        Self::annotate_network_locations(
            &client_mgr,
            Self::get_user_id(&auth_session)?,
            machine_id,
            &mut response,
        )
        .await;
        Ok(Json(response))
    }

    async fn handle_list_network_instance_ids(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path(machine_id): Path<uuid::Uuid>,
    ) -> Result<Json<ListNetworkInstanceIdsJsonResp>, HttpHandleError> {
        Ok(client_mgr
            .handle_list_network_instance_ids((Self::get_user_id(&auth_session)?, machine_id))
            .await
            .map_err(convert_error)?
            .into())
    }

    async fn handle_remove_network_instance(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path((machine_id, inst_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    ) -> Result<(), HttpHandleError> {
        let user_id = Self::get_user_id(&auth_session)?;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        let result = client_mgr
            .handle_remove_network_instances((user_id, machine_id), vec![inst_id])
            .await;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        result.map_err(convert_error)?;
        Ok(())
    }

    async fn handle_list_machines(
        auth_session: AuthSession,
        State(client_mgr): AppState,
    ) -> Result<Json<ListMachineJsonResp>, HttpHandleError> {
        let user_id = Self::get_user_id(&auth_session)?;

        let client_urls = client_mgr.list_machine_by_user_id(user_id).await;

        let mut machines = vec![];
        for item in client_urls.iter() {
            let client_url = item.clone();
            let session = client_mgr.get_heartbeat_requests(&client_url).await;
            if let Some(info) = &session
                && !info.running_network_instances.is_empty()
                && let Some(machine_id) = info.machine_id.map(uuid::Uuid::from)
                && let Some(permit) = client_mgr
                    .begin_network_location_refresh(user_id, machine_id)
                    .await
            {
                let manager = client_mgr.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Ok(Ok(mut response)) = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        manager.handle_collect_network_info((user_id, machine_id), None),
                    )
                    .await
                    {
                        Self::annotate_network_locations(
                            &manager,
                            user_id,
                            machine_id,
                            &mut response,
                        )
                        .await;
                    }
                });
            }
            let cached_location = client_mgr.cached_network_location(&client_url).await;
            let (public_ip, location) = cached_location
                .map(|(ip, location)| (Some(ip), Some(location)))
                .unwrap_or_default();
            machines.push(ListMachineItem {
                client_url: Some(client_url),
                // Location refreshes are bounded and cached; the transport
                // URL is never used as the node address.
                public_ip,
                info: session,
                location,
            });
        }

        Ok(Json(ListMachineJsonResp { machines }))
    }

    async fn handle_update_network_state(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path((machine_id, inst_id)): Path<(uuid::Uuid, Option<uuid::Uuid>)>,
        Json(payload): Json<UpdateNetworkStateJsonReq>,
    ) -> Result<(), HttpHandleError> {
        let Some(inst_id) = inst_id else {
            // not implement disable all
            return Err((
                StatusCode::NOT_IMPLEMENTED,
                other_error("Not implemented".to_string()).into(),
            ));
        };

        let user_id = Self::get_user_id(&auth_session)?;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        let result = client_mgr
            .handle_update_network_state((user_id, machine_id), inst_id, payload.disabled)
            .await;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        result.map_err(convert_error)?;
        Ok(())
    }

    async fn handle_get_network_metas(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path(machine_id): Path<uuid::Uuid>,
        Json(payload): Json<GetNetworkMetasJsonReq>,
    ) -> Result<Json<GetNetworkMetasResponse>, HttpHandleError> {
        Ok(Json(
            client_mgr
                .handle_get_network_metas(
                    (Self::get_user_id(&auth_session)?, machine_id),
                    payload.instance_ids,
                )
                .await
                .map_err(convert_error)?,
        ))
    }

    async fn handle_save_network_config(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path((machine_id, inst_id)): Path<(uuid::Uuid, uuid::Uuid)>,
        Json(payload): Json<SaveNetworkJsonReq>,
    ) -> Result<(), HttpHandleError> {
        if payload.config.instance_id() != inst_id.to_string() {
            return Err((
                StatusCode::BAD_REQUEST,
                other_error("Instance ID mismatch".to_string()).into(),
            ));
        }
        let user_id = Self::get_user_id(&auth_session)?;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        let result = client_mgr
            .handle_save_network_config_with_source(
                (user_id, machine_id),
                inst_id,
                payload.config,
                RuntimeConfigSource::Web,
            )
            .await;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        result.map_err(convert_error)?;
        Ok(())
    }

    async fn handle_get_network_config(
        auth_session: AuthSession,
        State(client_mgr): AppState,
        Path((machine_id, inst_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    ) -> Result<Json<NetworkConfig>, HttpHandleError> {
        let user_id = Self::get_user_id(&auth_session)?;
        Ok(client_mgr
            .handle_get_network_config((user_id, machine_id), inst_id)
            .await
            .map_err(convert_error)?
            .into())
    }

    // --- Token-authenticated machine-scoped handlers (no AuthSession) ---

    async fn handle_run_network_instance_internal(
        State(client_mgr): AppState,
        Path((user_id, machine_id)): Path<(UserIdInDb, uuid::Uuid)>,
        Json(payload): Json<RunNetworkJsonReq>,
    ) -> Result<Json<Void>, HttpHandleError> {
        let source = payload
            .source
            .and_then(config_source_from_rpc)
            .unwrap_or(RuntimeConfigSource::Web);
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        let result = client_mgr
            .handle_run_network_instance_with_source(
                (user_id, machine_id),
                payload.config,
                payload.save,
                source,
            )
            .await;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        result.map_err(convert_error)?;
        Ok(Void::default().into())
    }

    async fn handle_remove_network_instance_internal(
        State(client_mgr): AppState,
        Path((user_id, machine_id, inst_id)): Path<(UserIdInDb, uuid::Uuid, uuid::Uuid)>,
    ) -> Result<(), HttpHandleError> {
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        let result = client_mgr
            .handle_remove_network_instances((user_id, machine_id), vec![inst_id])
            .await;
        client_mgr
            .invalidate_applied_config_revision(user_id, machine_id)
            .await;
        result.map_err(convert_error)?;
        Ok(())
    }

    async fn handle_reconcile_managed_network_configs_internal(
        State(client_mgr): AppState,
        Path((user_id, machine_id)): Path<(UserIdInDb, uuid::Uuid)>,
        Json(payload): Json<ReconcileManagedNetworkConfigsJsonReq>,
    ) -> Result<StatusCode, HttpHandleError> {
        let desired = payload
            .managed_network_configs
            .into_iter()
            .map(|item| crate::webhook::ManagedNetworkConfig {
                instance_id: item.instance_id.to_string(),
                network_config: item.network_config,
            })
            .collect();
        client_mgr
            .reconcile_managed_network_configs(
                user_id,
                machine_id,
                desired,
                payload.config_revision,
                payload.expected_config_revision,
            )
            .await
            .map_err(Self::convert_managed_config_error)?;
        Ok(StatusCode::NO_CONTENT)
    }

    async fn handle_patch_managed_network_configs_internal(
        State(client_mgr): AppState,
        Path((user_id, machine_id)): Path<(UserIdInDb, uuid::Uuid)>,
        Json(payload): Json<PatchManagedNetworkConfigsJsonReq>,
    ) -> Result<StatusCode, HttpHandleError> {
        let upserts = payload
            .upserts
            .into_iter()
            .map(|item| crate::webhook::ManagedNetworkConfig {
                instance_id: item.instance_id.to_string(),
                network_config: item.network_config,
            })
            .collect();
        client_mgr
            .patch_managed_network_configs(
                user_id,
                machine_id,
                upserts,
                payload.delete_instance_ids,
                payload.config_revision,
                payload.expected_config_revision,
            )
            .await
            .map_err(Self::convert_managed_config_error)?;
        Ok(StatusCode::NO_CONTENT)
    }

    async fn handle_list_network_instance_ids_internal(
        State(client_mgr): AppState,
        Path((user_id, machine_id)): Path<(UserIdInDb, uuid::Uuid)>,
    ) -> Result<Json<ListNetworkInstanceIdsJsonResp>, HttpHandleError> {
        Ok(client_mgr
            .handle_list_network_instance_ids((user_id, machine_id))
            .await
            .map_err(convert_error)?
            .into())
    }

    async fn handle_collect_network_info_internal(
        State(client_mgr): AppState,
        Path((user_id, machine_id)): Path<(UserIdInDb, uuid::Uuid)>,
        Json(payload): Json<CollectNetworkInfoJsonReq>,
    ) -> Result<Json<CollectNetworkInfoResponse>, HttpHandleError> {
        let mut response = client_mgr
            .handle_collect_network_info((user_id, machine_id), payload.inst_ids)
            .await
            .map_err(convert_error)?
            .into();
        Self::annotate_network_locations(&client_mgr, user_id, machine_id, &mut response).await;
        Ok(Json(response))
    }

    pub fn build_route_internal() -> Router<AppStateInner> {
        Router::new()
            .route(
                "/api/internal/users/{user-id}/machines/{machine-id}/networks",
                put(Self::handle_reconcile_managed_network_configs_internal)
                    .patch(Self::handle_patch_managed_network_configs_internal)
                    .layer(DefaultBodyLimit::max(MAX_MANAGED_CONFIG_REQUEST_BODY_SIZE))
                    .post(Self::handle_run_network_instance_internal)
                    .get(Self::handle_list_network_instance_ids_internal),
            )
            .route(
                "/api/internal/users/{user-id}/machines/{machine-id}/networks/{inst-id}",
                delete(Self::handle_remove_network_instance_internal),
            )
            .route(
                "/api/internal/users/{user-id}/machines/{machine-id}/networks/info",
                get(Self::handle_collect_network_info_internal),
            )
    }

    pub fn build_route() -> Router<AppStateInner> {
        Router::new()
            .route("/api/v1/machines", get(Self::handle_list_machines))
            .route(
                "/api/v1/machines/{machine-id}/validate-config",
                post(Self::handle_validate_config),
            )
            .route(
                "/api/v1/machines/{machine-id}/networks",
                post(Self::handle_run_network_instance).get(Self::handle_list_network_instance_ids),
            )
            .route(
                "/api/v1/machines/{machine-id}/networks/{inst-id}",
                delete(Self::handle_remove_network_instance).put(Self::handle_update_network_state),
            )
            .route(
                "/api/v1/machines/{machine-id}/networks/info",
                get(Self::handle_collect_network_info).post(Self::handle_collect_network_info),
            )
            .route(
                "/api/v1/machines/{machine-id}/networks/info/{inst-id}",
                get(Self::handle_collect_one_network_info),
            )
            .route(
                "/api/v1/machines/{machine-id}/networks/config/{inst-id}",
                get(Self::handle_get_network_config).put(Self::handle_save_network_config),
            )
            .route(
                "/api/v1/machines/{machine-id}/networks/metas",
                post(Self::handle_get_network_metas),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_public_ip_prefers_network_stun_address() {
        let node = MyNodeInfo {
            stun_info: Some(easytier::proto::common::StunInfo {
                public_ip: vec!["203.0.113.10".to_string()],
                ..Default::default()
            }),
            ips: Some(easytier::proto::peer_rpc::GetIpListResponse {
                public_ipv4: Some(std::net::Ipv4Addr::new(198, 51, 100, 20).into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            NetworkApi::node_public_ip(&node),
            Some("203.0.113.10".parse().unwrap())
        );
    }

    #[test]
    fn node_public_ip_ignores_unusable_stun_addresses() {
        let node = MyNodeInfo {
            stun_info: Some(easytier::proto::common::StunInfo {
                public_ip: vec![
                    "proxy.example.com".to_string(),
                    "0.0.0.0".to_string(),
                    "127.0.0.1".to_string(),
                    "10.0.0.1".to_string(),
                    "100.64.0.1".to_string(),
                    "fd00::1".to_string(),
                ],
                ..Default::default()
            }),
            ips: Some(easytier::proto::peer_rpc::GetIpListResponse {
                public_ipv4: Some(std::net::Ipv4Addr::new(198, 51, 100, 20).into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            NetworkApi::node_public_ip(&node),
            Some("198.51.100.20".parse().unwrap())
        );
    }

    #[test]
    fn node_public_ip_rejects_default_or_loopback_node_addresses() {
        let empty = MyNodeInfo {
            ips: Some(easytier::proto::peer_rpc::GetIpListResponse {
                public_ipv4: Some(std::net::Ipv4Addr::UNSPECIFIED.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(NetworkApi::node_public_ip(&empty), None);

        let local = MyNodeInfo {
            ips: Some(easytier::proto::peer_rpc::GetIpListResponse {
                public_ipv4: Some(std::net::Ipv4Addr::LOCALHOST.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(NetworkApi::node_public_ip(&local), None);
    }

    #[test]
    fn revision_conflict_response_exposes_machine_readable_current_revision() {
        let error = crate::client_manager::ManagedConfigError::RevisionConflict {
            expected: Some("rev-1".to_string()),
            current: Some("rev-2".to_string()),
        };

        let (status, Json(body)) = NetworkApi::convert_managed_config_error(error.into());

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body.code.as_deref(),
            Some("managed_config_revision_conflict")
        );
        assert_eq!(body.current_config_revision.as_deref(), Some("rev-2"));
    }

    #[test]
    fn ownership_conflict_is_distinct_from_revision_conflict() {
        let error = crate::client_manager::ManagedConfigError::OwnershipConflict {
            instance_id: uuid::Uuid::new_v4(),
        };

        let (status, Json(body)) = NetworkApi::convert_managed_config_error(error.into());

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body.code.as_deref(),
            Some("managed_config_ownership_conflict")
        );
        assert_eq!(body.current_config_revision, None);
    }
}
