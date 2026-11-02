use uuid::Uuid;

use super::{Db, UserIdInDb, sqlx_db_error};

/// The core settings last seen on one instance of a device.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct MeshObservation {
    pub device_id: String,
    pub instance_id: String,
    pub network_name: String,
    pub core: String,
    pub sync_at: Option<i64>,
    pub credential: bool,
}

/// A core change an offline member missed.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct MeshCoreReplay {
    pub device_id: String,
    pub instance_id: String,
    pub base: String,
    pub base_sync_at: Option<i64>,
    pub change: String,
    pub target_sync_at: i64,
}

fn now() -> String {
    sqlx::types::chrono::Local::now()
        .fixed_offset()
        .to_rfc3339()
}

impl Db {
    pub(crate) async fn list_user_device_ids(
        &self,
        user_id: UserIdInDb,
    ) -> Result<Vec<Uuid>, sea_orm::DbErr> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT machine_id FROM devices WHERE user_id = ? ORDER BY machine_id",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await
        .map_err(sqlx_db_error)?;
        Ok(ids
            .into_iter()
            .filter_map(|id| Uuid::parse_str(&id).ok())
            .collect())
    }

    /// Replace one device's observations with what its catalog holds now.
    pub(crate) async fn replace_mesh_observations(
        &self,
        user_id: UserIdInDb,
        device_id: Uuid,
        observations: &[MeshObservation],
    ) -> Result<(), sea_orm::DbErr> {
        let mut transaction = self
            .db
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(sqlx_db_error)?;
        sqlx::query("DELETE FROM mesh_observations WHERE user_id = ? AND device_id = ?")
            .bind(user_id)
            .bind(device_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_db_error)?;
        let seen_at = now();
        for observation in observations {
            sqlx::query(
                "INSERT INTO mesh_observations \
                 (user_id, device_id, instance_id, network_name, core, sync_at, credential, seen_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(user_id)
            .bind(device_id.to_string())
            .bind(&observation.instance_id)
            .bind(&observation.network_name)
            .bind(&observation.core)
            .bind(observation.sync_at)
            .bind(observation.credential)
            .bind(&seen_at)
            .execute(&mut *transaction)
            .await
            .map_err(sqlx_db_error)?;
        }
        transaction.commit().await.map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn list_mesh_observations(
        &self,
        user_id: UserIdInDb,
        network_name: Option<&str>,
    ) -> Result<Vec<MeshObservation>, sea_orm::DbErr> {
        sqlx::query_as(
            "SELECT device_id, instance_id, network_name, core, sync_at, credential \
             FROM mesh_observations WHERE user_id = ? AND (? IS NULL OR network_name = ?) \
             ORDER BY device_id, instance_id",
        )
        .bind(user_id)
        .bind(network_name)
        .bind(network_name)
        .fetch_all(&self.db)
        .await
        .map_err(sqlx_db_error)
    }

    pub(crate) async fn list_mesh_core_replays(
        &self,
        user_id: UserIdInDb,
    ) -> Result<Vec<MeshCoreReplay>, sea_orm::DbErr> {
        sqlx::query_as(
            "SELECT device_id, instance_id, base, base_sync_at, change, target_sync_at \
             FROM mesh_core_replays WHERE user_id = ? ORDER BY device_id, instance_id",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await
        .map_err(sqlx_db_error)
    }

    pub(crate) async fn put_mesh_core_replay(
        &self,
        user_id: UserIdInDb,
        replay: &MeshCoreReplay,
    ) -> Result<(), sea_orm::DbErr> {
        sqlx::query(
            "INSERT INTO mesh_core_replays \
             (user_id, device_id, instance_id, base, base_sync_at, change, target_sync_at, create_time) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, device_id, instance_id) DO UPDATE SET \
             change = excluded.change, target_sync_at = excluded.target_sync_at",
        )
        .bind(user_id)
        .bind(&replay.device_id)
        .bind(&replay.instance_id)
        .bind(&replay.base)
        .bind(replay.base_sync_at)
        .bind(&replay.change)
        .bind(replay.target_sync_at)
        .bind(now())
        .execute(&self.db)
        .await
        .map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn delete_mesh_core_replay(
        &self,
        user_id: UserIdInDb,
        device_id: &str,
        instance_id: &str,
    ) -> Result<(), sea_orm::DbErr> {
        sqlx::query(
            "DELETE FROM mesh_core_replays WHERE user_id = ? AND device_id = ? AND instance_id = ?",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(instance_id)
        .execute(&self.db)
        .await
        .map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn mesh_credential_delivery(
        &self,
        user_id: UserIdInDb,
        device_id: &str,
        instance_id: &str,
    ) -> Result<Vec<String>, sea_orm::DbErr> {
        let ids: Option<String> = sqlx::query_scalar(
            "SELECT credential_ids FROM mesh_credential_deliveries \
             WHERE user_id = ? AND device_id = ? AND instance_id = ?",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(instance_id)
        .fetch_optional(&self.db)
        .await
        .map_err(sqlx_db_error)?;
        Ok(ids
            .and_then(|ids| serde_json::from_str(&ids).ok())
            .unwrap_or_default())
    }

    pub(crate) async fn put_mesh_credential_delivery(
        &self,
        user_id: UserIdInDb,
        device_id: &str,
        instance_id: &str,
        network_id: Uuid,
        credential_ids: &[String],
    ) -> Result<(), sea_orm::DbErr> {
        let ids = serde_json::to_string(credential_ids)
            .map_err(|error| sea_orm::DbErr::Json(error.to_string()))?;
        sqlx::query(
            "INSERT INTO mesh_credential_deliveries \
             (user_id, device_id, instance_id, network_id, credential_ids) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, device_id, instance_id) DO UPDATE SET \
             network_id = excluded.network_id, credential_ids = excluded.credential_ids",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(instance_id)
        .bind(network_id.to_string())
        .bind(ids)
        .execute(&self.db)
        .await
        .map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn has_mesh_credential_deliveries(
        &self,
        user_id: UserIdInDb,
        network_id: Uuid,
    ) -> Result<bool, sea_orm::DbErr> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mesh_credential_deliveries \
             WHERE user_id = ? AND network_id = ? AND credential_ids != '[]'",
        )
        .bind(user_id)
        .bind(network_id.to_string())
        .fetch_one(&self.db)
        .await
        .map_err(sqlx_db_error)?;
        Ok(count > 0)
    }
}
