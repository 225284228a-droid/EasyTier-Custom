use easytier::proto::api::manage::PatchPersistedConfigRequest;
use uuid::Uuid;

use super::{
    ClientManager,
    session::local_configs::{LocalConfigError, LocalConfigView},
};
use crate::db::UserIdInDb;

impl ClientManager {
    /// The node's config directory, read live. An offline node has none.
    pub(crate) async fn local_configs(
        &self,
        user_id: UserIdInDb,
        machine_id: Uuid,
    ) -> anyhow::Result<Option<LocalConfigView>> {
        let Some(session) = self.get_session_by_machine_id(user_id, &machine_id) else {
            return Ok(None);
        };
        if !session.has_local_revision_capability().await {
            return Ok(None);
        }
        session.local_configs().await.map(Some)
    }

    pub(crate) async fn patch_local_config(
        &self,
        user_id: UserIdInDb,
        machine_id: Uuid,
        request: PatchPersistedConfigRequest,
    ) -> Result<LocalConfigView, LocalConfigError> {
        let session = self
            .get_session_by_machine_id(user_id, &machine_id)
            .ok_or(LocalConfigError::Offline)?;
        session.patch_local_config(request).await
    }

    pub(crate) async fn mutate_local_config_lifecycle(
        &self,
        user_id: UserIdInDb,
        machine_id: Uuid,
        instance: Uuid,
        expected_revision: String,
        enabled: Option<bool>,
    ) -> Result<LocalConfigView, LocalConfigError> {
        self.get_session_by_machine_id(user_id, &machine_id)
            .ok_or(LocalConfigError::Offline)?
            .mutate_local_config_lifecycle(instance, expected_revision, enabled)
            .await
    }
}
