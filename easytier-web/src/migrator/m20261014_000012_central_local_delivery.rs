use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20261014_000012_central_local_delivery"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE central_local_config_deliveries (
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                device_id TEXT NOT NULL,
                instance_id TEXT NOT NULL,
                desired_config TEXT,
                delivered_config TEXT,
                revision TEXT,
                baseline TEXT,
                pending_operation INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                PRIMARY KEY (user_id, device_id, instance_id)
            );",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE central_local_config_deliveries;")
            .await?;
        Ok(())
    }
}
