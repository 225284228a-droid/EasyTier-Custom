use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20261108_000017_preset_without_wg"
    }
}

/// The listener list presets were created with before WireGuard became opt-in.
const OLD_DEFAULT: &str =
    r#""listeners":["tcp://0.0.0.0:11010","udp://0.0.0.0:11010","wg://0.0.0.0:11011"]"#;
const NEW_DEFAULT: &str = r#""listeners":["tcp://0.0.0.0:11010","udp://0.0.0.0:11010"]"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    /// Presets still holding the old default listen on TCP and UDP only, like
    /// new networks. A preset whose listeners were edited is left alone.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&format!(
                "UPDATE mesh_networks SET preset = REPLACE(preset, '{OLD_DEFAULT}', '{NEW_DEFAULT}') \
                 WHERE INSTR(preset, '{OLD_DEFAULT}') > 0;"
            ))
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait as _, Database};
    use sea_orm_migration::MigratorTrait as _;

    use crate::migrator::Migrator;

    #[tokio::test]
    async fn old_default_presets_drop_wireguard_and_edited_ones_stay() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let before = Migrator::migrations()
            .iter()
            .position(|migration| migration.name() == "m20261108_000017_preset_without_wg")
            .unwrap() as u32;
        Migrator::up(&db, Some(before)).await.unwrap();
        db.execute_unprepared(
            r#"INSERT INTO users (id, username, password) VALUES (900, 'migration-owner', 'x');
            INSERT INTO mesh_networks (user_id, network_name, core, preset, create_time) VALUES
                (900, 'old', '{}', '{"peers":[],"listeners":["tcp://0.0.0.0:11010","udp://0.0.0.0:11010","wg://0.0.0.0:11011"],"advanced":{}}', 't'),
                (900, 'edited', '{}', '{"peers":[],"listeners":["wg://0.0.0.0:11011"],"advanced":{}}', 't');"#,
        )
        .await
        .unwrap();
        Migrator::up(&db, None).await.unwrap();

        let preset = |name: &'static str| {
            let db = &db;
            async move {
                db.query_one(sea_orm::Statement::from_string(
                    db.get_database_backend(),
                    format!("SELECT preset FROM mesh_networks WHERE network_name = '{name}'"),
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get_by_index::<String>(0)
                .unwrap()
            }
        };
        assert_eq!(
            preset("old").await,
            r#"{"peers":[],"listeners":["tcp://0.0.0.0:11010","udp://0.0.0.0:11010"],"advanced":{}}"#
        );
        assert_eq!(
            preset("edited").await,
            r#"{"peers":[],"listeners":["wg://0.0.0.0:11011"],"advanced":{}}"#
        );
    }
}
