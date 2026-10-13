use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use easytier::proto::api::manage::PatchPersistedConfigRequest;
use uuid::Uuid;

use super::{AppStateInner, Error, HttpHandleError, authed_user_id, users::AuthSession};
use crate::{
    client_manager::session::local_configs::LocalConfigError,
    db::local_config_mirror::LocalConfigMirror,
};

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
        LocalConfigError::Invalid(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "local_config_invalid_patch",
            None,
        ),
        LocalConfigError::NotFound(_) => (StatusCode::NOT_FOUND, "local_config_not_found", None),
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

pub(super) fn router() -> Router<AppStateInner> {
    Router::new()
        .route("/api/v1/machines/{machine}/local-configs", get(observe))
        .route(
            "/api/v1/machines/{machine}/local-configs/patch",
            post(patch),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_patch_retains_the_public_diagnostic_and_existing_error_code() {
        let (status, Json(error)) = failure(LocalConfigError::Invalid(
            "field is not editable (fields: enable_vpn_portal)".into(),
        ));
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.code.as_deref(), Some("local_config_invalid_patch"));
        assert_eq!(
            error.message,
            "invalid configuration patch: field is not editable (fields: enable_vpn_portal)"
        );
        assert!(error.current_config_revision.is_none());
    }

    #[test]
    fn missing_patch_targets_return_not_found_instead_of_invalid_patch() {
        let (status, Json(error)) = failure(LocalConfigError::NotFound(
            "configuration does not exist".into(),
        ));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error.code.as_deref(), Some("local_config_not_found"));
        assert_eq!(
            error.message,
            "configuration not found: configuration does not exist"
        );
        assert!(error.current_config_revision.is_none());
    }
}
