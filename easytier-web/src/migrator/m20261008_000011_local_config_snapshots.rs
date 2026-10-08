use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20261008_000011_local_config_snapshots"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE local_config_snapshots (
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                device_id TEXT NOT NULL,
                observer_epoch TEXT NOT NULL,
                session_epoch INTEGER NOT NULL,
                catalog_epoch TEXT NOT NULL,
                catalog_generation INTEGER NOT NULL,
                snapshot_json TEXT NOT NULL,
                capabilities_json TEXT NOT NULL,
                observed_at TEXT NOT NULL,
                PRIMARY KEY (user_id, device_id)
            );
            CREATE TABLE local_config_nodes (
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                device_id TEXT NOT NULL,
                observer_epoch TEXT NOT NULL,
                session_epoch INTEGER NOT NULL,
                local_config INTEGER NOT NULL,
                PRIMARY KEY (user_id, device_id)
            );",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE local_config_nodes; DROP TABLE local_config_snapshots;")
            .await?;
        Ok(())
    }
}
