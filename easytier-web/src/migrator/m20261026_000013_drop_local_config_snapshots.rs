use sea_orm_migration::prelude::*;

/// Node configurations live only in each node's config directory; the console
/// reads them live instead of keeping a copy.
pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20261026_000013_drop_local_config_snapshots"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS local_config_snapshots;")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
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
            );",
            )
            .await?;
        Ok(())
    }
}
