//! Whether a device manages its configurations in its own config directory.
//! Remembered while the device is offline so central publication never
//! queues console-owned configurations for it. No configuration is stored.
use sea_orm::DbErr;
use uuid::Uuid;

use super::{Db, UserIdInDb, sqlx_db_error};

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn modes_are_tenant_scoped_and_reject_old_sessions() {
        let db = Db::memory_db().await;
        let user = db.auto_create_user("mode-owner").await.unwrap();
        let other = db.auto_create_user("mode-other").await.unwrap();
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
        // Session counters restart with the web server. Its new observer
        // epoch lets a fresh connection replace the remembered mode.
        db.store_local_config_mode(user.id, machine, Uuid::new_v4(), 1, false)
            .await
            .unwrap();
        assert!(!db.known_local_config_mode(user.id, machine).await.unwrap());
    }
}
