use std::{io::Write as _, path::Path};

use atomic_write_file::{AtomicWriteFile, OpenOptions};

use easytier_core::management::{ConfigFileControl, ConfigFilePermission, ConfigFileStorage};

#[derive(Default)]
pub(crate) struct NativeConfigFileStorage {
    disable_env_parsing: bool,
}

impl NativeConfigFileStorage {
    pub(crate) fn new(disable_env_parsing: bool) -> Self {
        Self {
            disable_env_parsing,
        }
    }
}

#[async_trait::async_trait]
impl ConfigFileStorage for NativeConfigFileStorage {
    fn supports_catalog(&self) -> bool {
        true
    }

    async fn list_configs(&self, directory: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
        super::persisted_config_policy::list_configs(directory).await
    }

    async fn inspect_raw_config(
        &self,
        path: &Path,
        contents: &[u8],
        config_dir: &Path,
    ) -> anyhow::Result<ConfigFileControl> {
        super::persisted_config_policy::inspect_raw(
            path,
            contents,
            config_dir,
            self.disable_env_parsing,
        )
        .await
    }

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
        if tokio::fs::symlink_metadata(path)
            .await
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            anyhow::bail!("symbolic configuration links are protected");
        }
        match tokio::fs::read(path).await {
            Ok(contents) => Ok(Some(contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn load_config(
        &self,
        path: &Path,
        config_dir: Option<&Path>,
    ) -> anyhow::Result<Option<(easytier_core::config::toml::TomlConfig, ConfigFileControl)>> {
        let Some(raw) = self.read(path).await? else {
            return Ok(None);
        };
        let raw_control = super::persisted_config_policy::inspect_raw(
            path,
            &raw,
            config_dir
                .or_else(|| path.parent())
                .unwrap_or(Path::new("")),
            self.disable_env_parsing,
        )
        .await?;
        match crate::common::config::load_config_from_file(
            &path.to_path_buf(),
            config_dir.map(Path::to_path_buf).as_ref(),
            self.disable_env_parsing,
        )
        .await
        {
            Ok((config, mut control)) => {
                control.permission = ConfigFilePermission::from(
                    u8::from(control.permission) | u8::from(raw_control.permission),
                );
                Ok(Some((config, control)))
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(_) => anyhow::bail!("configuration could not be loaded"),
        }
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> anyhow::Result<()> {
        let path = path.to_owned();
        let contents = contents.to_owned();
        tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            let mut options = OpenOptions::new();
            #[cfg(not(unix))]
            let options = OpenOptions::new();
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
        let storage = NativeConfigFileStorage::default();

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
        let storage = NativeConfigFileStorage::default();

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
        let storage = NativeConfigFileStorage::default();
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
            Arc::new(NativeConfigFileStorage::default()),
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

    #[tokio::test]
    async fn semantic_loader_preserves_env_permissions_through_enable_and_delete() {
        use crate::instance::factory::native_instance_manager_with_config_dir;
        use easytier_core::{
            config::toml::ConfigLoader as _,
            management::{InstanceStateStore, ProcessManagement},
        };
        use std::sync::Arc;

        let directory = tempfile::tempdir().unwrap();
        let config_dir = directory.path().to_path_buf();
        let id = uuid::Uuid::new_v4();
        let path = config_dir.join(format!("{id}.toml"));
        let original = format!(
            "instance_id = \"{id}\"\ninstance_name = \"env-protected\"\nlisteners = []\n[network_identity]\nnetwork_name = \"env-network\"\nnetwork_secret = \"${{EASYTIER_STORAGE_TEST_SECRET:-fallback-secret}}\"\n[flags]\nno_tun = true\n"
        );
        std::fs::write(&path, &original).unwrap();
        let storage = Arc::new(NativeConfigFileStorage::default());
        assert!(!storage.inspect(&path).await.is_read_only());
        let (config, control) = storage
            .load_config(&path, Some(&config_dir))
            .await
            .unwrap()
            .unwrap();
        assert!(control.is_read_only());
        assert!(!control.is_deletable());
        assert_ne!(
            config.get_network_identity().network_secret.unwrap(),
            "${EASYTIER_STORAGE_TEST_SECRET:-fallback-secret}"
        );
        let manager = Arc::new(native_instance_manager_with_config_dir(Some(
            config_dir.clone(),
        )));
        manager.register_managed_config(id, control).unwrap();
        let state = Arc::new(InstanceStateStore::new(Some(&config_dir)));
        state.set_enabled(id, false).unwrap();
        let process = ProcessManagement::new(
            manager.clone(),
            Arc::new(()),
            storage.clone(),
            state.clone(),
        );
        process
            .set_network_instance_enabled(id, true)
            .await
            .unwrap();
        let result = process.delete_network_instances(vec![id]).await.unwrap();
        assert_eq!(result.remaining_instance_ids, vec![id]);
        assert!(manager.instance(id).is_some());
        assert!(
            process
                .remove_network_instance_configs(&[id])
                .await
                .unwrap()
                .is_empty()
        );
        process
            .set_network_instance_enabled(id, false)
            .await
            .unwrap();
        assert!(
            process
                .remove_network_instance_configs(&[id])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!state.is_enabled(&id));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let (_, unexpanded_control) = NativeConfigFileStorage::new(true)
            .load_config(&path, Some(&config_dir))
            .await
            .unwrap()
            .unwrap();
        assert!(!unexpanded_control.is_read_only());
        assert!(unexpanded_control.is_deletable());
    }
}
