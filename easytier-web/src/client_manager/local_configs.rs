use easytier::proto::api::manage::PatchPersistedConfigRequest;
use uuid::Uuid;

use super::{ClientManager, session::local_configs::LocalConfigError};
use crate::db::{UserIdInDb, local_config_mirror::LocalConfigMirror};

impl ClientManager {
    pub(crate) async fn local_config_mirror(
        &self,
        user_id: UserIdInDb,
        machine_id: Uuid,
    ) -> anyhow::Result<Option<LocalConfigMirror>> {
        if let Some(session) = self.get_session_by_machine_id(user_id, &machine_id)
            && session.has_local_revision_capability().await
        {
            return session.observe_local_configs().await.map(Some);
        }
        // Cached mirrors contain only observations, and remain stale offline.
        Ok(self
            .storage
            .db()
            .local_config_mirror(user_id, machine_id)
            .await?)
    }

    pub(crate) async fn local_config_mirrors(
        &self,
        user_id: UserIdInDb,
    ) -> anyhow::Result<Vec<LocalConfigMirror>> {
        let mut mirrors = self.storage.db().local_config_mirrors(user_id).await?;
        for mirror in &mut mirrors {
            if let Some(session) = self.get_session_by_machine_id(user_id, &mirror.machine_id) {
                mirror.online = true;
                if let Some((epoch, generation, capabilities)) =
                    session.local_catalog_binding().await
                {
                    // The background worker keeps the database current. Listing
                    // all nodes does not serialize a series of remote RPC reads.
                    mirror.capabilities = capabilities;
                    mirror.stale = mirror.snapshot.catalog_epoch != epoch
                        || mirror.snapshot.catalog_generation < generation
                        || chrono::DateTime::parse_from_rfc3339(&mirror.observed_at)
                            .map(|time| {
                                chrono::Utc::now().signed_duration_since(time).num_seconds() > 35
                            })
                            .unwrap_or(true);
                }
            }
        }
        Ok(mirrors)
    }

    pub(crate) async fn patch_local_config(
        &self,
        user_id: UserIdInDb,
        machine_id: Uuid,
        request: PatchPersistedConfigRequest,
    ) -> Result<LocalConfigMirror, LocalConfigError> {
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
    ) -> Result<LocalConfigMirror, LocalConfigError> {
        self.get_session_by_machine_id(user_id, &machine_id)
            .ok_or(LocalConfigError::Offline)?
            .mutate_local_config_lifecycle(instance, expected_revision, enabled)
            .await
    }
}
