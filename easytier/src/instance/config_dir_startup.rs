//! Startup scanning for managed configuration directories (`config.d`).
//!
//! Shared by the CLI startup path and the GUI's local backend so both apply
//! the same isolation policy: invalid files and duplicate instance UUIDs
//! are skipped with a warning while every other valid configuration keeps
//! loading, and an entry whose identity cannot be confirmed defers
//! disabled-state cleanup instead of dropping persisted stop state.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use easytier_core::config::toml::ConfigLoader as _;
use easytier_core::config::toml::TomlConfigLoader;
use easytier_core::management::{ConfigFileControl, ConfigFileStorage, InstanceStateStore};

use crate::common::config::{load_config_from_contents, load_config_from_file};

use super::config_storage::NativeConfigFileStorage;

pub struct StartupConfigFile {
    pub path: PathBuf,
    pub config: TomlConfigLoader,
    pub control: ConfigFileControl,
    pub original_contents: Option<Vec<u8>>,
}

#[derive(Default)]
pub struct StartupConfigs {
    pub files: Vec<StartupConfigFile>,
    pub config_dir_file_count: usize,
    pub config_dir_ids: HashSet<uuid::Uuid>,
    pub unidentified_config_dir_entry: bool,
}

impl StartupConfigs {
    pub fn gc_disabled_state(&self, state: &InstanceStateStore) {
        // A file can still own a stopped instance even when its flags no
        // longer parse. An unidentified/unreadable file prevents proving
        // that any of the old identities have actually disappeared.
        if self.unidentified_config_dir_entry {
            tracing::warn!(
                "Deferring instance state cleanup: a config directory entry has no confirmed identity"
            );
            return;
        }
        for id in state.disabled_instance_ids() {
            if !self.config_dir_ids.contains(&id)
                && let Err(error) = state.remove(&id)
            {
                tracing::warn!(%error, %id, "failed to GC stale instance state entry");
            }
        }
    }
}

pub fn config_document_identity(contents: &[u8]) -> Option<uuid::Uuid> {
    let table = toml::from_str::<toml::Table>(std::str::from_utf8(contents).ok()?).ok()?;
    table
        .get("instance_id")?
        .as_str()?
        .parse::<uuid::Uuid>()
        .ok()
}

/// Scans a managed configuration directory with the startup isolation
/// policy: unreadable or invalid entries are skipped with a warning, and
/// entries whose instance UUID is claimed by more than one file are all
/// excluded, so a single bad file never blocks the remaining networks.
pub async fn load_config_dir_entries(
    directory: &Path,
    disable_env_parsing: bool,
) -> anyhow::Result<StartupConfigs> {
    let mut startup = StartupConfigs::default();
    if !directory.is_dir() {
        anyhow::bail!("config_dir {} is not a directory", directory.display());
    }
    let storage = NativeConfigFileStorage::new(disable_env_parsing);
    let paths = storage.list_configs(directory).await?;
    // Count even invalid and conflicting entries, so they never cause an
    // implicit default network to be created instead.
    startup.config_dir_file_count = paths.len();
    let config_dir = directory.to_path_buf();
    let mut owners = HashMap::<uuid::Uuid, HashSet<PathBuf>>::new();
    for path in paths {
        let filename_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<uuid::Uuid>().ok());
        if let Some(id) = filename_id {
            startup.config_dir_ids.insert(id);
        }
        // Inspect identity without expanding environment values or asking
        // get_id() to generate an identity for a legacy document.
        let original_contents = match storage.read(&path).await {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(%error, config_file = ?path, "Could not inspect config directory identity");
                None
            }
        };
        let raw_id = original_contents
            .as_deref()
            .and_then(config_document_identity);
        if let Some(id) = raw_id.or(filename_id) {
            startup.config_dir_ids.insert(id);
            owners.entry(id).or_default().insert(path.clone());
        } else {
            startup.unidentified_config_dir_entry = true;
        }
        // Use the original native loader: valid read-only and environment
        // configurations can still start locally despite remote protection.
        let loaded = if let Some(raw) = &original_contents {
            match String::from_utf8(raw.clone()) {
                Ok(contents) => {
                    load_config_from_contents(
                        &path,
                        contents,
                        Some(&config_dir),
                        disable_env_parsing,
                    )
                    .await
                }
                Err(error) => Err(error.into()),
            }
        } else {
            load_config_from_file(&path, Some(&config_dir), disable_env_parsing).await
        };
        let (config, control) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(%error, config_file = ?path, "Skipping invalid config directory entry");
                continue;
            }
        };
        // The native loader may resolve an environment-supplied UUID or
        // materialize a legacy runtime UUID while assigning permissions.
        // Reading its dump does not generate another UUID or write the file.
        let loaded_id = config_document_identity(config.dump().as_bytes());
        if let Some(id) = loaded_id {
            startup.config_dir_ids.insert(id);
            owners.entry(id).or_default().insert(path.clone());
        }
        if raw_id.is_some() && raw_id != loaded_id {
            tracing::warn!(config_file = ?path, "Skipping config directory entry whose identity changed while loading");
            continue;
        }
        startup.files.push(StartupConfigFile {
            path,
            config,
            control,
            original_contents,
        });
    }
    let mut conflicting_paths = HashSet::new();
    for (id, paths) in owners.into_iter().filter(|(_, paths)| paths.len() > 1) {
        tracing::warn!(%id, config_files = ?paths, "Skipping config directory entries with a duplicate instance UUID");
        conflicting_paths.extend(paths);
    }
    startup
        .files
        .retain(|file| !conflicting_paths.contains(&file.path));
    Ok(startup)
}
