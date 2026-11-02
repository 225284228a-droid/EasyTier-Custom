use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20261103_000015_mesh_networks"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "-- What this console keeps for a network it manages: the core
                -- settings it last wrote, the preset for devices that join
                -- and whether new devices join on their own.
                CREATE TABLE mesh_networks (
                    user_id INTEGER NOT NULL,
                    network_name TEXT NOT NULL,
                    core TEXT NOT NULL,
                    sync_at INTEGER,
                    preset TEXT NOT NULL,
                    auto_join_since INTEGER,
                    create_time TEXT NOT NULL,
                    PRIMARY KEY (user_id, network_name),
                    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
                );

                -- Devices that already joined a network on their own, so
                -- each joins once.
                CREATE TABLE mesh_auto_joins (
                    user_id INTEGER NOT NULL,
                    network_name TEXT NOT NULL,
                    device_id TEXT NOT NULL,
                    PRIMARY KEY (user_id, network_name, device_id),
                    FOREIGN KEY (user_id, device_id)
                        REFERENCES devices(user_id, machine_id)
                        ON DELETE CASCADE ON UPDATE CASCADE
                );

                -- Instances whose advanced settings and running state this
                -- console's batch edits leave alone.
                CREATE TABLE mesh_locks (
                    user_id INTEGER NOT NULL,
                    device_id TEXT NOT NULL,
                    instance_id TEXT NOT NULL,
                    PRIMARY KEY (user_id, device_id, instance_id),
                    FOREIGN KEY (user_id, device_id)
                        REFERENCES devices(user_id, machine_id)
                        ON DELETE CASCADE ON UPDATE CASCADE
                );

                -- Advanced settings an offline member missed, sent once when
                -- it next connects unless another console wrote later.
                CREATE TABLE mesh_advanced_replays (
                    user_id INTEGER NOT NULL,
                    device_id TEXT NOT NULL,
                    instance_id TEXT NOT NULL,
                    change TEXT NOT NULL,
                    target_sync_at INTEGER NOT NULL,
                    create_time TEXT NOT NULL,
                    PRIMARY KEY (user_id, device_id, instance_id),
                    FOREIGN KEY (user_id, device_id)
                        REFERENCES devices(user_id, machine_id)
                        ON DELETE CASCADE ON UPDATE CASCADE
                );

                -- Members of a deleted network that were offline: paused
                -- once when they next connect.
                CREATE TABLE mesh_pause_replays (
                    user_id INTEGER NOT NULL,
                    device_id TEXT NOT NULL,
                    instance_id TEXT NOT NULL,
                    PRIMARY KEY (user_id, device_id, instance_id),
                    FOREIGN KEY (user_id, device_id)
                        REFERENCES devices(user_id, machine_id)
                        ON DELETE CASCADE ON UPDATE CASCADE
                );

                ALTER TABLE mesh_observations ADD COLUMN advanced TEXT NOT NULL DEFAULT '{}';
                ALTER TABLE mesh_observations ADD COLUMN advanced_sync_at INTEGER;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE mesh_observations DROP COLUMN advanced_sync_at;
                ALTER TABLE mesh_observations DROP COLUMN advanced;
                DROP TABLE mesh_pause_replays;
                DROP TABLE mesh_advanced_replays;
                DROP TABLE mesh_locks;
                DROP TABLE mesh_auto_joins;
                DROP TABLE mesh_networks;",
            )
            .await?;
        Ok(())
    }
}
