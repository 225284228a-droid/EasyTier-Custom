use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use easytier::proto::api::manage::PatchPersistedConfigRequest;
use uuid::Uuid;

use super::{AppStateInner, Error, HttpHandleError, authed_user_id, users::AuthSession};
use crate::{
    client_manager::session::local_configs::LocalConfigError,
    db::local_config_mirror::LocalConfigMirror,
};

#[derive(serde::Serialize)]
struct Mirrors {
    machines: Vec<LocalConfigMirror>,
}

pub(super) fn failure(error: LocalConfigError) -> HttpHandleError {
    let (status, code, revision) = match &error {
        LocalConfigError::Offline => (
            StatusCode::SERVICE_UNAVAILABLE,
            "local_config_offline",
            None,
        ),
        LocalConfigError::Unsupported => (StatusCode::CONFLICT, "local_config_unsupported", None),
        LocalConfigError::Conflict(revision) => (
            StatusCode::CONFLICT,
            "local_config_revision_conflict",
            revision.clone(),
        ),
        LocalConfigError::Protected => (StatusCode::FORBIDDEN, "local_config_protected", None),
        LocalConfigError::Invalid => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "local_config_invalid_patch",
            None,
        ),
        LocalConfigError::Unknown => (
            StatusCode::GATEWAY_TIMEOUT,
            "local_config_outcome_unknown",
            None,
        ),
        LocalConfigError::ApplyFailed => {
            (StatusCode::BAD_GATEWAY, "local_config_apply_failed", None)
        }
        LocalConfigError::Observation => (
            StatusCode::BAD_GATEWAY,
            "local_config_observation_failed",
            None,
        ),
    };
    (
        status,
        Json(Error {
            message: error.to_string(),
            code: Some(code.into()),
            current_config_revision: revision,
        }),
    )
}

async fn list(
    auth: AuthSession,
    State(manager): State<AppStateInner>,
) -> Result<Json<Mirrors>, HttpHandleError> {
    let user = authed_user_id(&auth)?;
    let machines = manager
        .local_config_mirrors(user)
        .await
        .map_err(|_| failure(LocalConfigError::Observation))?;
    Ok(Json(Mirrors { machines }))
}

async fn observe(
    auth: AuthSession,
    State(manager): State<AppStateInner>,
    Path(machine): Path<Uuid>,
) -> Result<Json<LocalConfigMirror>, HttpHandleError> {
    let user = authed_user_id(&auth)?;
    let mirror = manager
        .local_config_mirror(user, machine)
        .await
        .map_err(|_| failure(LocalConfigError::Observation))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(Error {
                    message: "no observed local configuration catalog".into(),
                    code: Some("local_config_not_found".into()),
                    current_config_revision: None,
                }),
            )
        })?;
    Ok(Json(mirror))
}

async fn patch(
    auth: AuthSession,
    State(manager): State<AppStateInner>,
    Path(machine): Path<Uuid>,
    Json(request): Json<PatchPersistedConfigRequest>,
) -> Result<Json<LocalConfigMirror>, HttpHandleError> {
    let user = authed_user_id(&auth)?;
    Ok(Json(
        manager
            .patch_local_config(user, machine, request)
            .await
            .map_err(failure)?,
    ))
}

#[derive(serde::Deserialize)]
struct LifecycleRequest {
    expected_revision: String,
    enabled: Option<bool>,
}

async fn set_enabled(
    auth: AuthSession,
    State(manager): State<AppStateInner>,
    Path((machine, instance)): Path<(Uuid, Uuid)>,
    Json(request): Json<LifecycleRequest>,
) -> Result<Json<LocalConfigMirror>, HttpHandleError> {
    let enabled = request
        .enabled
        .ok_or_else(|| failure(LocalConfigError::Invalid))?;
    Ok(Json(
        manager
            .mutate_local_config_lifecycle(
                authed_user_id(&auth)?,
                machine,
                instance,
                request.expected_revision,
                Some(enabled),
            )
            .await
            .map_err(failure)?,
    ))
}

async fn remove(
    auth: AuthSession,
    State(manager): State<AppStateInner>,
    Path((machine, instance)): Path<(Uuid, Uuid)>,
    Json(request): Json<LifecycleRequest>,
) -> Result<Json<LocalConfigMirror>, HttpHandleError> {
    Ok(Json(
        manager
            .mutate_local_config_lifecycle(
                authed_user_id(&auth)?,
                machine,
                instance,
                request.expected_revision,
                None,
            )
            .await
            .map_err(failure)?,
    ))
}

pub(super) fn router() -> Router<AppStateInner> {
    Router::new()
        .route("/api/v1/local-configs", get(list))
        .route("/api/v1/machines/{machine}/local-configs", get(observe))
        .route(
            "/api/v1/machines/{machine}/local-configs/patch",
            post(patch),
        )
        .route(
            "/api/v1/machines/{machine}/local-configs/{instance}/enabled",
            post(set_enabled),
        )
        .route(
            "/api/v1/machines/{machine}/local-configs/{instance}",
            delete(remove),
        )
}
