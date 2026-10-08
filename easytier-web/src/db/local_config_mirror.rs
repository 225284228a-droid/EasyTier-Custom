//! Observations of node-owned TOML. These rows never enter a publication queue.
use easytier::proto::api::manage::ObserveConfigsResponse;
use sea_orm::DbErr;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use super::{Db, UserIdInDb, sqlx_db_error};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LocalConfigMirror {
    pub machine_id: Uuid,
    #[serde(flatten)]
    pub snapshot: ObserveConfigsResponse,
    pub capabilities: Vec<String>,
    pub observed_at: String,
    pub online: bool,
    pub stale: bool,
}

fn decode_row(row: sqlx::sqlite::SqliteRow) -> Result<LocalConfigMirror, DbErr> {
    let snapshot: String = row.try_get("snapshot_json").map_err(sqlx_db_error)?;
    let capabilities: String = row.try_get("capabilities_json").map_err(sqlx_db_error)?;
    let device_id: String = row.try_get("device_id").map_err(sqlx_db_error)?;
    Ok(LocalConfigMirror {
        machine_id: device_id
            .parse()
            .map_err(|error: uuid::Error| DbErr::Custom(error.to_string()))?,
        snapshot: serde_json::from_str(&snapshot)
            .map_err(|error| DbErr::Json(error.to_string()))?,
        capabilities: serde_json::from_str(&capabilities)
            .map_err(|error| DbErr::Json(error.to_string()))?,
        observed_at: row.try_get("observed_at").map_err(sqlx_db_error)?,
        online: false,
        stale: true,
    })
}

impl Db {
    pub(crate) async fn store_local_config_mode(
        &self,
        user_id: UserIdInDb,
        machine_id: Uuid,
        observer_epoch: Uuid,
        session_epoch: u64,
        local_config: bool,
    ) -> Result<(), DbErr> {
        let session_epoch = i64::try_from(session_epoch)
            .map_err(|_| DbErr::Custom("session epoch overflow".into()))?;
        sqlx::query(
            "INSERT INTO local_config_nodes (user_id, device_id, observer_epoch, session_epoch, local_config)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id, device_id) DO UPDATE SET
                observer_epoch = excluded.observer_epoch, session_epoch = excluded.session_epoch,
                local_config = excluded.local_config
             WHERE excluded.observer_epoch <> local_config_nodes.observer_epoch
                OR excluded.session_epoch >= local_config_nodes.session_epoch"
        ).bind(user_id).bind(machine_id.to_string()).bind(observer_epoch.to_string())
            .bind(session_epoch).bind(local_config).execute(&self.db).await.map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn known_local_config_mode(
        &self,
        user_id: UserIdInDb,
        machine_id: Uuid,
    ) -> Result<bool, DbErr> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT local_config FROM local_config_nodes WHERE user_id = ? AND device_id = ?",
        )
        .bind(user_id)
        .bind(machine_id.to_string())
        .fetch_optional(&self.db)
        .await
        .map_err(sqlx_db_error)?
        .unwrap_or(false))
    }

    /// A complete observed catalog is one row, so readers never combine entries
    /// from different generations. Session ownership is checked by the caller
    /// under its per-device mutation lock; SQL also rejects delayed old results.
    pub(crate) async fn store_local_config_snapshot(
        &self,
        user_id: UserIdInDb,
        machine_id: Uuid,
        observer_epoch: Uuid,
        session_epoch: u64,
        snapshot: &ObserveConfigsResponse,
        capabilities: &[String],
    ) -> Result<bool, DbErr> {
        let session_epoch = i64::try_from(session_epoch)
            .map_err(|_| DbErr::Custom("session epoch overflow".into()))?;
        let generation = i64::try_from(snapshot.catalog_generation)
            .map_err(|_| DbErr::Custom("catalog generation overflow".into()))?;
        let serialized =
            serde_json::to_string(snapshot).map_err(|error| DbErr::Json(error.to_string()))?;
        let capabilities =
            serde_json::to_string(capabilities).map_err(|error| DbErr::Json(error.to_string()))?;
        let result = sqlx::query(
            "INSERT INTO local_config_snapshots
             (user_id, device_id, observer_epoch, session_epoch, catalog_epoch, catalog_generation, snapshot_json, capabilities_json, observed_at)
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?
             WHERE NOT EXISTS (
               SELECT 1 FROM local_config_nodes
               WHERE user_id = ? AND device_id = ? AND observer_epoch = ?
                 AND (session_epoch > ? OR local_config = 0)
             )
             ON CONFLICT(user_id, device_id) DO UPDATE SET
               observer_epoch = excluded.observer_epoch,
               session_epoch = excluded.session_epoch,
               catalog_epoch = excluded.catalog_epoch,
               catalog_generation = excluded.catalog_generation,
               snapshot_json = excluded.snapshot_json,
               capabilities_json = excluded.capabilities_json,
               observed_at = excluded.observed_at
             WHERE excluded.observer_epoch <> local_config_snapshots.observer_epoch
                OR excluded.session_epoch > local_config_snapshots.session_epoch
                OR (excluded.session_epoch = local_config_snapshots.session_epoch
                    AND excluded.catalog_epoch = local_config_snapshots.catalog_epoch
                    AND excluded.catalog_generation >= local_config_snapshots.catalog_generation)",
        )
        .bind(user_id).bind(machine_id.to_string()).bind(observer_epoch.to_string()).bind(session_epoch)
        .bind(&snapshot.catalog_epoch).bind(generation).bind(serialized).bind(capabilities)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(user_id).bind(machine_id.to_string()).bind(observer_epoch.to_string()).bind(session_epoch)
        .execute(&self.db).await.map_err(sqlx_db_error)?;
        Ok(result.rows_affected() > 0)
    }

    pub(crate) async fn local_config_mirror(
        &self,
        user_id: UserIdInDb,
        machine_id: Uuid,
    ) -> Result<Option<LocalConfigMirror>, DbErr> {
        let row =
            sqlx::query("SELECT * FROM local_config_snapshots WHERE user_id = ? AND device_id = ?")
                .bind(user_id)
                .bind(machine_id.to_string())
                .fetch_optional(&self.db)
                .await
                .map_err(sqlx_db_error)?;
        row.map(decode_row).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_new_authenticated_session_fences_reads_before_its_first_snapshot() {
        let db = Db::memory_db().await;
        let user = db.auto_create_user("mirror-owner").await.unwrap();
        let machine = Uuid::new_v4();
        let observer = Uuid::new_v4();
        let snapshot = ObserveConfigsResponse {
            catalog_epoch: "old-process".into(),
            catalog_generation: 90,
            entries: vec![],
        };
        db.store_local_config_mode(user.id, machine, observer, 2, true)
            .await
            .unwrap();
        assert!(
            !db.store_local_config_snapshot(user.id, machine, observer, 1, &snapshot, &[])
                .await
                .unwrap()
        );
        assert!(
            db.local_config_mirror(user.id, machine)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.store_local_config_snapshot(user.id, machine, observer, 2, &snapshot, &[])
                .await
                .unwrap()
        );
        db.store_local_config_mode(user.id, machine, observer, 3, false)
            .await
            .unwrap();
        assert!(
            !db.store_local_config_snapshot(user.id, machine, observer, 3, &snapshot, &[])
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn mirrors_are_tenant_scoped_and_reject_old_sessions_and_generations() {
        let db = Db::memory_db().await;
        let user = db.auto_create_user("mirror-owner").await.unwrap();
        let other = db.auto_create_user("mirror-other").await.unwrap();
        let machine = Uuid::new_v4();
        let observer = Uuid::new_v4();
        db.store_local_config_mode(user.id, machine, observer, 10, true)
            .await
            .unwrap();
        db.store_local_config_mode(user.id, machine, observer, 9, false)
            .await
            .unwrap();
        assert!(db.known_local_config_mode(user.id, machine).await.unwrap());
        assert!(!db.known_local_config_mode(other.id, machine).await.unwrap());
        let mut snapshot = ObserveConfigsResponse {
            catalog_epoch: "process-a".into(),
            catalog_generation: 2,
            entries: vec![],
        };
        assert!(
            db.store_local_config_snapshot(user.id, machine, observer, 10, &snapshot, &[])
                .await
                .unwrap()
        );
        snapshot.catalog_generation = 1;
        assert!(
            !db.store_local_config_snapshot(user.id, machine, observer, 10, &snapshot, &[])
                .await
                .unwrap()
        );
        snapshot.catalog_epoch = "process-b".into();
        assert!(
            db.store_local_config_snapshot(user.id, machine, observer, 11, &snapshot, &[])
                .await
                .unwrap()
        );
        snapshot.catalog_epoch = "process-a".into();
        snapshot.catalog_generation = 99;
        assert!(
            !db.store_local_config_snapshot(user.id, machine, observer, 10, &snapshot, &[])
                .await
                .unwrap()
        );
        assert_eq!(
            db.local_config_mirror(user.id, machine)
                .await
                .unwrap()
                .unwrap()
                .snapshot
                .catalog_epoch,
            "process-b"
        );
        assert!(
            db.local_config_mirror(other.id, machine)
                .await
                .unwrap()
                .is_none()
        );
        // Session counters restart when the web server restarts. Its independent
        // observer epoch permits a fresh connection to replace the old mirror.
        assert!(
            db.store_local_config_snapshot(user.id, machine, Uuid::new_v4(), 1, &snapshot, &[])
                .await
                .unwrap()
        );
    }
}
