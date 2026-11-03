use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20261104_000016_mesh_network_acl"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "-- The network part of each instance's ACL, as `MemberAcl` JSON.
                ALTER TABLE mesh_observations ADD COLUMN acl TEXT NOT NULL DEFAULT '{}';",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE mesh_observations DROP COLUMN acl;")
            .await?;
        Ok(())
    }
}
