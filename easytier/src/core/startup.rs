use super::{ConfigFileSource, NetworkOptions};
use crate::common::config::{
    ConfigFileControl, ConfigLoader, TomlConfigLoader, load_config_from_file,
};
use crate::common::log;
use crate::instance::factory::NativeInstanceManager;
use anyhow::Context;
use easytier_core::management::InstanceStateStore;
use std::path::PathBuf;

pub(super) struct StartupConfigFile {
    pub(super) path: PathBuf,
    pub(super) source: ConfigFileSource,
    pub(super) config: TomlConfigLoader,
    pub(super) control: ConfigFileControl,
    pub(super) original_contents: Option<Vec<u8>>,
}

#[derive(Default)]
pub(super) struct StartupConfigs {
    pub(super) files: Vec<StartupConfigFile>,
    pub(super) explicit_file_count: usize,
    pub(super) directory: crate::instance::config_dir_startup::StartupConfigs,
}

impl StartupConfigs {
    pub(super) fn reconcile_state(&self, state: &InstanceStateStore) {
        self.directory.reconcile_state(state)
    }
}

pub(super) async fn prepare_startup_configs(
    explicit_files: &[PathBuf],
    config_dir: Option<&PathBuf>,
    disable_env_parsing: bool,
) -> anyhow::Result<StartupConfigs> {
    let mut startup = StartupConfigs {
        explicit_file_count: explicit_files.len(),
        ..Default::default()
    };
    // Explicit --config-file errors retain their fatal startup behavior.
    for path in explicit_files {
        let original_contents = if path.as_os_str() == "-" {
            None
        } else {
            Some(
                tokio::fs::read(path)
                    .await
                    .with_context(|| format!("failed to read config file {}", path.display()))?,
            )
        };
        let (config, control) = match original_contents.as_ref() {
            Some(raw) => {
                crate::common::config::load_config_from_contents(
                    path,
                    String::from_utf8(raw.clone())?,
                    config_dir,
                    disable_env_parsing,
                )
                .await?
            }
            None => load_config_from_file(path, config_dir, disable_env_parsing).await?,
        };
        startup.files.push(StartupConfigFile {
            path: path.clone(),
            source: ConfigFileSource::CliConfigFile,
            config,
            control,
            original_contents,
        });
    }
    let Some(directory) = config_dir else {
        return Ok(startup);
    };
    // The config directory is scanned with the shared isolation policy
    // (also used by the GUI local backend): invalid entries and duplicate
    // instance UUIDs are skipped instead of failing the startup.
    let mut entries = crate::instance::config_dir_startup::load_config_dir_entries(
        directory,
        disable_env_parsing,
    )
    .await?;
    startup
        .files
        .extend(entries.files.drain(..).map(|file| StartupConfigFile {
            path: file.path,
            source: ConfigFileSource::ConfigDir,
            config: file.config,
            control: file.control,
            original_contents: file.original_contents,
        }));
    startup.directory = entries;
    Ok(startup)
}

pub(super) fn start_prepared_configs(
    startup: &mut StartupConfigs,
    manager: &NativeInstanceManager,
    state: &InstanceStateStore,
    options: &NetworkOptions,
    mut create_cli_network: bool,
) -> anyhow::Result<bool> {
    for StartupConfigFile {
        path,
        source,
        config,
        mut control,
        original_contents,
    } in startup.files.drain(..)
    {
        let result = (|| -> anyhow::Result<()> {
            if source == ConfigFileSource::ConfigDir {
                manager.register_managed_config(config.get_id(), control.clone())?;
                if !state.is_enabled(&config.get_id()) {
                    if let Some(contents) = original_contents.as_deref() {
                        manager
                            .local_config_catalog()
                            .acknowledge_file(&path, contents);
                    }
                    log::info!(config_file = ?path, id = %config.get_id(), "Skipping config stopped before shutdown; enable it from the web console to run again");
                    return Ok(());
                }
            }
            if options.can_merge(
                &config,
                source,
                startup.explicit_file_count,
                startup.directory.config_dir_file_count,
            ) {
                options
                    .merge_into(&config)
                    .with_context(|| format!("failed to merge config from cli: {path:?}"))?;
                create_cli_network = false;
                control.set_read_only(true);
                control.set_no_delete(true);
            }
            log::info!(
                "Starting easytier from config file {:?}({:?}) with config:\n############### TOML ###############\n{}\n-----------------------------------\n",
                path,
                control.permission,
                config.dump_redacted()
            );
            if source == ConfigFileSource::ConfigDir {
                manager.register_managed_config(config.get_id(), control.clone())?;
            }
            manager.run_network_instance(config, control)?;
            if let Some(contents) = original_contents.as_deref() {
                manager
                    .local_config_catalog()
                    .acknowledge_file(&path, contents);
            }
            Ok(())
        })();
        if let Err(error) = result {
            if source == ConfigFileSource::CliConfigFile {
                return Err(error);
            }
            log::warn!(%error, config_file = ?path, "Skipping config directory entry that could not be started");
        }
    }
    Ok(create_cli_network)
}
