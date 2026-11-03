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
    /// The advanced settings it held, as `AdvancedValues` JSON.
    pub advanced: String,
    pub advanced_sync_at: Option<i64>,
    /// The network part of its ACL, as `MemberAcl` JSON.
    pub acl: String,
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

/// What this console keeps for one network it manages.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct MeshNetworkRecord {
    pub network_name: String,
    /// `CoreSettings` JSON this console last wrote.
    pub core: String,
    pub sync_at: Option<i64>,
    /// `Preset` JSON for devices that join.
    pub preset: String,
    /// UTC milliseconds since new devices join on their own; unset is off.
    pub auto_join_since: Option<i64>,
}

/// Advanced settings an offline member missed.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct MeshAdvancedReplay {
    pub device_id: String,
    pub instance_id: String,
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
                 (user_id, device_id, instance_id, network_name, core, sync_at, credential, advanced, \
                 advanced_sync_at, acl, seen_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(user_id)
            .bind(device_id.to_string())
            .bind(&observation.instance_id)
            .bind(&observation.network_name)
            .bind(&observation.core)
            .bind(observation.sync_at)
            .bind(observation.credential)
            .bind(&observation.advanced)
            .bind(observation.advanced_sync_at)
            .bind(&observation.acl)
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
            "SELECT device_id, instance_id, network_name, core, sync_at, credential, advanced, advanced_sync_at, acl \
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

    pub(crate) async fn list_mesh_networks(
        &self,
        user_id: UserIdInDb,
    ) -> Result<Vec<MeshNetworkRecord>, sea_orm::DbErr> {
        sqlx::query_as(
            "SELECT network_name, core, sync_at, preset, auto_join_since \
             FROM mesh_networks WHERE user_id = ? ORDER BY create_time, network_name",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await
        .map_err(sqlx_db_error)
    }

    pub(crate) async fn get_mesh_network(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
    ) -> Result<Option<MeshNetworkRecord>, sea_orm::DbErr> {
        sqlx::query_as(
            "SELECT network_name, core, sync_at, preset, auto_join_since \
             FROM mesh_networks WHERE user_id = ? AND network_name = ?",
        )
        .bind(user_id)
        .bind(network_name)
        .fetch_optional(&self.db)
        .await
        .map_err(sqlx_db_error)
    }

    pub(crate) async fn put_mesh_network(
        &self,
        user_id: UserIdInDb,
        record: &MeshNetworkRecord,
    ) -> Result<(), sea_orm::DbErr> {
        sqlx::query(
            "INSERT INTO mesh_networks \
             (user_id, network_name, core, sync_at, preset, auto_join_since, create_time) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, network_name) DO UPDATE SET \
             core = excluded.core, sync_at = excluded.sync_at, preset = excluded.preset, \
             auto_join_since = excluded.auto_join_since",
        )
        .bind(user_id)
        .bind(&record.network_name)
        .bind(&record.core)
        .bind(record.sync_at)
        .bind(&record.preset)
        .bind(record.auto_join_since)
        .bind(now())
        .execute(&self.db)
        .await
        .map_err(sqlx_db_error)?;
        Ok(())
    }

    /// Follow a rename; a record already holding the new name is kept.
    pub(crate) async fn rename_mesh_network(
        &self,
        user_id: UserIdInDb,
        from: &str,
        to: &str,
    ) -> Result<(), sea_orm::DbErr> {
        let mut transaction = self
            .db
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(sqlx_db_error)?;
        for sql in [
            "UPDATE OR IGNORE mesh_networks SET network_name = ? WHERE user_id = ? AND network_name = ?",
            "UPDATE OR IGNORE mesh_auto_joins SET network_name = ? WHERE user_id = ? AND network_name = ?",
        ] {
            sqlx::query(sql)
                .bind(to)
                .bind(user_id)
                .bind(from)
                .execute(&mut *transaction)
                .await
                .map_err(sqlx_db_error)?;
        }
        for sql in [
            "DELETE FROM mesh_networks WHERE user_id = ? AND network_name = ?",
            "DELETE FROM mesh_auto_joins WHERE user_id = ? AND network_name = ?",
        ] {
            sqlx::query(sql)
                .bind(user_id)
                .bind(from)
                .execute(&mut *transaction)
                .await
                .map_err(sqlx_db_error)?;
        }
        transaction.commit().await.map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn delete_mesh_network(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
    ) -> Result<(), sea_orm::DbErr> {
        for sql in [
            "DELETE FROM mesh_networks WHERE user_id = ? AND network_name = ?",
            "DELETE FROM mesh_auto_joins WHERE user_id = ? AND network_name = ?",
        ] {
            sqlx::query(sql)
                .bind(user_id)
                .bind(network_name)
                .execute(&self.db)
                .await
                .map_err(sqlx_db_error)?;
        }
        Ok(())
    }

    /// Devices that joined `network_name` on their own, or were already
    /// known when joining on their own was switched on.
    pub(crate) async fn mesh_auto_joined(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
    ) -> Result<Vec<String>, sea_orm::DbErr> {
        sqlx::query_scalar(
            "SELECT device_id FROM mesh_auto_joins WHERE user_id = ? AND network_name = ?",
        )
        .bind(user_id)
        .bind(network_name)
        .fetch_all(&self.db)
        .await
        .map_err(sqlx_db_error)
    }

    pub(crate) async fn put_mesh_auto_joins(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
        device_ids: &[String],
    ) -> Result<(), sea_orm::DbErr> {
        for device_id in device_ids {
            sqlx::query(
                "INSERT OR IGNORE INTO mesh_auto_joins (user_id, network_name, device_id) \
                 SELECT ?, ?, ? WHERE EXISTS \
                 (SELECT 1 FROM devices WHERE user_id = ? AND machine_id = ?)",
            )
            .bind(user_id)
            .bind(network_name)
            .bind(device_id)
            .bind(user_id)
            .bind(device_id)
            .execute(&self.db)
            .await
            .map_err(sqlx_db_error)?;
        }
        Ok(())
    }

    pub(crate) async fn list_mesh_locks(
        &self,
        user_id: UserIdInDb,
    ) -> Result<Vec<(String, String)>, sea_orm::DbErr> {
        sqlx::query_as("SELECT device_id, instance_id FROM mesh_locks WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&self.db)
            .await
            .map_err(sqlx_db_error)
    }

    pub(crate) async fn set_mesh_lock(
        &self,
        user_id: UserIdInDb,
        device_id: &str,
        instance_id: &str,
        locked: bool,
    ) -> Result<(), sea_orm::DbErr> {
        let sql = if locked {
            "INSERT OR IGNORE INTO mesh_locks (user_id, device_id, instance_id) VALUES (?, ?, ?)"
        } else {
            "DELETE FROM mesh_locks WHERE user_id = ? AND device_id = ? AND instance_id = ?"
        };
        sqlx::query(sql)
            .bind(user_id)
            .bind(device_id)
            .bind(instance_id)
            .execute(&self.db)
            .await
            .map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn list_mesh_advanced_replays(
        &self,
        user_id: UserIdInDb,
    ) -> Result<Vec<MeshAdvancedReplay>, sea_orm::DbErr> {
        sqlx::query_as(
            "SELECT device_id, instance_id, change, target_sync_at FROM mesh_advanced_replays \
             WHERE user_id = ? ORDER BY device_id, instance_id",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await
        .map_err(sqlx_db_error)
    }

    pub(crate) async fn put_mesh_advanced_replay(
        &self,
        user_id: UserIdInDb,
        replay: &MeshAdvancedReplay,
    ) -> Result<(), sea_orm::DbErr> {
        sqlx::query(
            "INSERT INTO mesh_advanced_replays \
             (user_id, device_id, instance_id, change, target_sync_at, create_time) VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, device_id, instance_id) DO UPDATE SET \
             change = excluded.change, target_sync_at = excluded.target_sync_at",
        )
        .bind(user_id)
        .bind(&replay.device_id)
        .bind(&replay.instance_id)
        .bind(&replay.change)
        .bind(replay.target_sync_at)
        .bind(now())
        .execute(&self.db)
        .await
        .map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn delete_mesh_advanced_replay(
        &self,
        user_id: UserIdInDb,
        device_id: &str,
        instance_id: &str,
    ) -> Result<(), sea_orm::DbErr> {
        sqlx::query(
            "DELETE FROM mesh_advanced_replays WHERE user_id = ? AND device_id = ? AND instance_id = ?",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(instance_id)
        .execute(&self.db)
        .await
        .map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn list_mesh_pause_replays(
        &self,
        user_id: UserIdInDb,
    ) -> Result<Vec<(String, String)>, sea_orm::DbErr> {
        sqlx::query_as("SELECT device_id, instance_id FROM mesh_pause_replays WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&self.db)
            .await
            .map_err(sqlx_db_error)
    }

    pub(crate) async fn set_mesh_pause_replay(
        &self,
        user_id: UserIdInDb,
        device_id: &str,
        instance_id: &str,
        pending: bool,
    ) -> Result<(), sea_orm::DbErr> {
        let sql = if pending {
            "INSERT OR IGNORE INTO mesh_pause_replays (user_id, device_id, instance_id) VALUES (?, ?, ?)"
        } else {
            "DELETE FROM mesh_pause_replays WHERE user_id = ? AND device_id = ? AND instance_id = ?"
        };
        sqlx::query(sql)
            .bind(user_id)
            .bind(device_id)
            .bind(instance_id)
            .execute(&self.db)
            .await
            .map_err(sqlx_db_error)?;
        Ok(())
    }
}
