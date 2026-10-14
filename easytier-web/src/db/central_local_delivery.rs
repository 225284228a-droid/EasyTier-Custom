use std::collections::BTreeMap;

use easytier::proto::api::manage::NetworkConfig;
use uuid::Uuid;

use super::{Db, UserIdInDb, sqlx_db_error};

/// This journal owns only identities created by central intent. A null desired
/// config is a durable deletion, surviving removal of the last member/device.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct CentralLocalDelivery {
    pub instance_id: String,
    pub desired_config: Option<String>,
    pub delivered_config: Option<String>,
    pub revision: Option<String>,
    pub baseline: Option<String>,
    pub pending_operation: bool,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn projections_retain_tombstones_and_fence_stale_delivery_results() {
        let db = Db::memory_db().await;
        let user = db
            .auto_create_user("central-local-delivery")
            .await
            .unwrap()
            .id;
        let other = db
            .auto_create_user("other-central-local-delivery")
            .await
            .unwrap()
            .id;
        let device = Uuid::new_v4();
        let instance = Uuid::new_v4();
        let config = |name: &str| {
            BTreeMap::from([(
                instance,
                NetworkConfig {
                    instance_id: Some(instance.to_string()),
                    hostname: Some(name.into()),
                    ..Default::default()
                },
            )])
        };
        let mut old = db
            .project_central_local_configs(user, device, &config("old"))
            .await
            .unwrap()
            .pop()
            .unwrap();
        db.project_central_local_configs(other, device, &config("other"))
            .await
            .unwrap();
        db.project_central_local_configs(user, device, &config("new"))
            .await
            .unwrap();
        old.delivered_config = old.desired_config.clone();
        old.revision = Some("stale-node-revision".into());
        db.update_central_local_delivery(user, device, &old)
            .await
            .unwrap();
        let current = db
            .project_central_local_configs(user, device, &config("new"))
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(current.delivered_config.is_none());
        assert!(current.revision.is_none());
        let retired = db
            .project_central_local_configs(user, device, &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(retired.len(), 1);
        assert!(retired[0].desired_config.is_none());
        let retained = db
            .project_central_local_configs(other, device, &config("other"))
            .await
            .unwrap();
        assert!(
            retained[0]
                .desired_config
                .as_ref()
                .unwrap()
                .contains("other")
        );
        db.remove_central_local_delivery(user, device, instance)
            .await
            .unwrap();
        assert!(
            db.project_central_local_configs(user, device, &BTreeMap::new())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.project_central_local_configs(other, device, &config("other"))
                .await
                .unwrap()
                .len(),
            1
        );
    }
}

impl Db {
    pub(crate) async fn project_central_local_configs(
        &self,
        user_id: UserIdInDb,
        device_id: Uuid,
        configs: &BTreeMap<Uuid, NetworkConfig>,
    ) -> Result<Vec<CentralLocalDelivery>, sea_orm::DbErr> {
        let mut transaction = self
            .db
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(sqlx_db_error)?;
        // Retire only identities already in this journal. Local/user files and
        // another console's identities are never inferred from a node catalog.
        sqlx::query("UPDATE central_local_config_deliveries SET desired_config = NULL WHERE user_id = ? AND device_id = ?")
            .bind(user_id).bind(device_id.to_string())
            .execute(&mut *transaction).await.map_err(sqlx_db_error)?;
        for (instance_id, config) in configs {
            let config = serde_json::to_string(config)
                .map_err(|error| sea_orm::DbErr::Json(error.to_string()))?;
            sqlx::query(
                "INSERT INTO central_local_config_deliveries (user_id, device_id, instance_id, desired_config) VALUES (?, ?, ?, ?) \
                 ON CONFLICT(user_id, device_id, instance_id) DO UPDATE SET desired_config = excluded.desired_config",
            )
            .bind(user_id).bind(device_id.to_string()).bind(instance_id.to_string()).bind(config)
            .execute(&mut *transaction).await.map_err(sqlx_db_error)?;
        }
        let rows = sqlx::query_as(
            "SELECT instance_id, desired_config, delivered_config, revision, baseline, pending_operation, error \
             FROM central_local_config_deliveries WHERE user_id = ? AND device_id = ? ORDER BY instance_id",
        )
        .bind(user_id).bind(device_id.to_string())
        .fetch_all(&mut *transaction).await.map_err(sqlx_db_error)?;
        transaction.commit().await.map_err(sqlx_db_error)?;
        Ok(rows)
    }

    pub(crate) async fn update_central_local_delivery(
        &self,
        user_id: UserIdInDb,
        device_id: Uuid,
        row: &CentralLocalDelivery,
    ) -> Result<bool, sea_orm::DbErr> {
        // An old in-flight RPC must not mark a later projection as delivered.
        let result = sqlx::query(
            "UPDATE central_local_config_deliveries SET delivered_config = ?, revision = ?, baseline = ?, pending_operation = ?, error = ? \
             WHERE user_id = ? AND device_id = ? AND instance_id = ? AND desired_config IS ?",
        )
        .bind(&row.delivered_config).bind(&row.revision).bind(&row.baseline).bind(row.pending_operation).bind(&row.error)
        .bind(user_id).bind(device_id.to_string()).bind(&row.instance_id).bind(&row.desired_config)
        .execute(&self.db).await.map_err(sqlx_db_error)?;
        Ok(result.rows_affected() == 1)
    }

    pub(crate) async fn remove_central_local_delivery(
        &self,
        user_id: UserIdInDb,
        device_id: Uuid,
        instance_id: Uuid,
    ) -> Result<(), sea_orm::DbErr> {
        sqlx::query("DELETE FROM central_local_config_deliveries WHERE user_id = ? AND device_id = ? AND instance_id = ? AND desired_config IS NULL")
            .bind(user_id).bind(device_id.to_string()).bind(instance_id.to_string())
            .execute(&self.db).await.map_err(sqlx_db_error)?;
        Ok(())
    }

    pub(crate) async fn central_local_delivery_error(
        &self,
        user_id: UserIdInDb,
        device_id: Uuid,
        instance_id: Uuid,
    ) -> Result<Option<String>, sea_orm::DbErr> {
        Ok(sqlx::query_scalar::<_, Option<String>>("SELECT error FROM central_local_config_deliveries WHERE user_id = ? AND device_id = ? AND instance_id = ?")
            .bind(user_id).bind(device_id.to_string()).bind(instance_id.to_string())
            .fetch_optional(&self.db).await.map_err(sqlx_db_error)?.flatten())
    }
}
