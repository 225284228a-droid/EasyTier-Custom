use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20261102_000014_mesh_core_sync"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE networks ADD COLUMN disable_encryption INTEGER NOT NULL DEFAULT 0
                    CHECK (disable_encryption IN (0, 1));
                ALTER TABLE networks ADD COLUMN encryption_algorithm TEXT;

                -- Central networks keep only temporary members. Permanent
                -- members leave central ownership: dropping their delivery
                -- journal first keeps their files on the nodes, because a
                -- journal row without desired config would delete them.
                DELETE FROM central_local_config_deliveries WHERE EXISTS (
                    SELECT 1 FROM network_members m
                    WHERE m.user_id = central_local_config_deliveries.user_id
                      AND m.device_id = central_local_config_deliveries.device_id
                      AND m.network_id = central_local_config_deliveries.instance_id
                      AND m.credential_id IS NULL
                );
                DELETE FROM network_members WHERE credential_id IS NULL;
                UPDATE networks SET acl_policy = NULL;

                -- The last core settings this console saw for each instance,
                -- so offline members can still be found and compared.
                CREATE TABLE mesh_observations (
                    user_id INTEGER NOT NULL,
                    device_id TEXT NOT NULL,
                    instance_id TEXT NOT NULL,
                    network_name TEXT NOT NULL,
                    core TEXT NOT NULL,
                    sync_at INTEGER,
                    credential INTEGER NOT NULL DEFAULT 0 CHECK (credential IN (0, 1)),
                    seen_at TEXT NOT NULL,
                    PRIMARY KEY (user_id, device_id, instance_id),
                    FOREIGN KEY (user_id, device_id)
                        REFERENCES devices(user_id, machine_id)
                        ON DELETE CASCADE ON UPDATE CASCADE
                );
                CREATE INDEX idx_mesh_observations_network
                    ON mesh_observations(user_id, network_name);

                -- A core change an offline member missed, sent once when it
                -- next connects.
                CREATE TABLE mesh_core_replays (
                    user_id INTEGER NOT NULL,
                    device_id TEXT NOT NULL,
                    instance_id TEXT NOT NULL,
                    base TEXT NOT NULL,
                    base_sync_at INTEGER,
                    change TEXT NOT NULL,
                    target_sync_at INTEGER NOT NULL,
                    create_time TEXT NOT NULL,
                    PRIMARY KEY (user_id, device_id, instance_id),
                    FOREIGN KEY (user_id, device_id)
                        REFERENCES devices(user_id, machine_id)
                        ON DELETE CASCADE ON UPDATE CASCADE
                );

                -- Temporary-node credentials this console placed on a
                -- permanent member, so later writes replace only its own.
                CREATE TABLE mesh_credential_deliveries (
                    user_id INTEGER NOT NULL,
                    device_id TEXT NOT NULL,
                    instance_id TEXT NOT NULL,
                    network_id TEXT NOT NULL,
                    credential_ids TEXT NOT NULL,
                    PRIMARY KEY (user_id, device_id, instance_id),
                    FOREIGN KEY (user_id, device_id)
                        REFERENCES devices(user_id, machine_id)
                        ON DELETE CASCADE ON UPDATE CASCADE
                );",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE mesh_credential_deliveries;
                DROP TABLE mesh_core_replays;
                DROP TABLE mesh_observations;
                ALTER TABLE networks DROP COLUMN encryption_algorithm;
                ALTER TABLE networks DROP COLUMN disable_encryption;",
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait as _, Database};
    use sea_orm_migration::MigratorTrait as _;

    use crate::migrator::Migrator;

    #[tokio::test]
    async fn permanent_members_leave_central_ownership_without_deleting_their_instances() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let before = Migrator::migrations().len() as u32 - 1;
        Migrator::up(&db, Some(before)).await.unwrap();
        db.execute_unprepared(
            "INSERT INTO users (id, username, password) VALUES (900, 'migration-owner', 'x');
            INSERT INTO devices (user_id, machine_id, first_seen_time, last_seen_time)
                VALUES (900, 'permanent', 't', 't'), (900, 'temporary', 't', 't');
            INSERT INTO networks (user_id, id, display_name, network_name, network_secret,
                networking_method, peer_urls, secure_mode, acl_policy, create_time, update_time)
                VALUES (900, 'net', 'Office', 'office', 's', 'Standalone', '[]', 1,
                    '{\"default_action\":\"deny\",\"rules\":[]}', 't', 't');
            INSERT INTO network_credentials (user_id, network_id, credential_id, credential_secret,
                expiry_unix, create_time, update_time) VALUES (900, 'net', 'cred', 'secret', 0, 't', 't');
            INSERT INTO network_members (user_id, network_id, id, device_id, credential_id,
                create_time, update_time) VALUES
                (900, 'net', 'm-permanent', 'permanent', NULL, 't', 't'),
                (900, 'net', 'm-temporary', 'temporary', 'cred', 't', 't');
            INSERT INTO central_local_config_deliveries (user_id, device_id, instance_id, desired_config)
                VALUES (900, 'permanent', 'net', '{}'), (900, 'temporary', 'net', '{}'),
                    (900, 'permanent', 'other-network', '{}');",
        )
        .await
        .unwrap();
        Migrator::up(&db, None).await.unwrap();

        let count = |sql: &'static str| {
            let db = &db;
            async move {
                db.query_one(sea_orm::Statement::from_string(
                    db.get_database_backend(),
                    sql,
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get_by_index::<i64>(0)
                .unwrap()
            }
        };
        // Without its journal row the permanent instance is no longer tracked
        // for deletion, so the node keeps it.
        assert_eq!(
            count("SELECT COUNT(*) FROM central_local_config_deliveries WHERE device_id = 'permanent' AND instance_id = 'net'").await,
            0
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM central_local_config_deliveries WHERE instance_id = 'other-network' OR device_id = 'temporary'").await,
            2
        );
        assert_eq!(count("SELECT COUNT(*) FROM network_members").await, 1);
        assert_eq!(
            count("SELECT COUNT(*) FROM network_members WHERE credential_id = 'cred'").await,
            1
        );
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM networks WHERE acl_policy IS NULL AND disable_encryption = 0"
            )
            .await,
            1
        );
    }
}
