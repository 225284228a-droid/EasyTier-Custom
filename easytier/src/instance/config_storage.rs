use std::{io::Write as _, path::Path};

use atomic_write_file::{AtomicWriteFile, OpenOptions};

use easytier_core::management::{ConfigFileControl, ConfigFilePermission, ConfigFileStorage};

#[derive(Default)]
pub(crate) struct NativeConfigFileStorage;

#[async_trait::async_trait]
impl ConfigFileStorage for NativeConfigFileStorage {
    async fn inspect(&self, path: &Path) -> ConfigFileControl {
        let permission = match tokio::fs::metadata(path).await {
            Ok(metadata) if metadata.permissions().readonly() => {
                ConfigFilePermission::from(ConfigFilePermission::READ_ONLY)
            }
            // A config file that does not exist yet carries no restrictions:
            // `save_network_instance_config` must be able to create it, which
            // is how the GUI and web console add new networks.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ConfigFilePermission::default()
            }
            Ok(_) => ConfigFilePermission::default(),
            Err(_) => ConfigFilePermission::from(ConfigFilePermission::READ_ONLY),
        };
        ConfigFileControl::new(Some(path.to_owned()), permission)
    }

    async fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        match tokio::fs::read(path).await {
            Ok(contents) => Ok(Some(contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> anyhow::Result<()> {
        let path = path.to_owned();
        let contents = contents.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut options = OpenOptions::new();
            #[cfg(unix)]
            {
                atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
                std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            }
            let mut file: AtomicWriteFile = options.open(path)?;
            file.write_all(&contents)?;
            file.commit()
        })
        .await??;
        Ok(())
    }

    async fn remove(&self, path: &Path) -> anyhow::Result<()> {
        tokio::fs::remove_file(path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn config_write_is_atomic_and_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("instance.toml");
        let storage = NativeConfigFileStorage;

        storage.write(&path, b"first").await.unwrap();
        storage.write(&path, b"second").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn missing_config_file_is_reported_as_creatable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("not-created-yet.toml");
        let storage = NativeConfigFileStorage;

        let control = storage.inspect(&path).await;
        assert!(
            !control.is_read_only(),
            "a config file that does not exist yet must be writable so that new \
             networks can be added from the GUI and web console"
        );
        assert!(control.is_deletable());
    }

    #[tokio::test]
    async fn read_only_config_file_is_reported_as_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("instance.toml");
        let storage = NativeConfigFileStorage;
        storage.write(&path, b"config").await.unwrap();

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();

        assert!(storage.inspect(&path).await.is_read_only());

        // Restore write permission so TempDir cleanup can delete the file.
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
    }

    /// Regression test for the web console / GUI "Create Network" flow: saving
    /// a config that does not exist on disk yet must create the TOML file and
    /// mark the instance as saved-but-disabled.
    #[tokio::test]
    async fn saving_a_new_config_file_creates_it_and_marks_it_disabled() {
        use std::sync::Arc;

        use easytier_core::management::{InstanceStateStore, ProcessManagement};

        use crate::common::config::{ConfigLoader as _, TomlConfigLoader};
        use crate::instance::factory::native_instance_manager_with_config_dir;

        let directory = tempfile::tempdir().unwrap();
        let config_dir = directory.path().to_path_buf();
        let manager = native_instance_manager_with_config_dir(Some(config_dir.clone()));
        let state_store = Arc::new(InstanceStateStore::new(Some(&config_dir)));
        let process = ProcessManagement::new(
            Arc::new(manager),
            Arc::new(()),
            Arc::new(NativeConfigFileStorage),
            state_store.clone(),
        );

        let config = TomlConfigLoader::default();
        let instance_id = config.get_id();
        config.set_inst_name("created-from-console".to_string());
        config.set_listeners(Vec::new());

        process
            .save_network_instance_config(config, Some(instance_id), None)
            .await
            .expect("saving a brand-new config file must not be rejected as read-only");

        let path = config_dir.join(format!("{instance_id}.toml"));
        assert!(path.is_file(), "config file {} must exist", path.display());
        assert!(
            !state_store.is_enabled(&instance_id),
            "a saved network starts disabled until it is explicitly enabled"
        );

        // Re-saving the same instance keeps the file on disk.
        let updated = TomlConfigLoader::default();
        updated.set_id(instance_id);
        updated.set_inst_name("created-from-console".to_string());
        updated.set_listeners(Vec::new());
        process
            .save_network_instance_config(updated, Some(instance_id), None)
            .await
            .unwrap();
        assert!(path.is_file());
    }
}
