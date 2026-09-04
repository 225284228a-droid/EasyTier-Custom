//! Persistent enabled/disabled state for config-dir managed instances.
//!
//! EasyTier-core keeps authoritative network configs as `*.toml` files under
//! its working (`--config-dir`) directory. This module records which of those
//! configs were running and which were stopped when the process shut down, so
//! the next startup can restore the exact run state.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

pub const STATE_FILE_NAME: &str = "easytier-state.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstanceState {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StateFile {
    version: u32,
    instances: HashMap<String, InstanceState>,
}

fn read_state_file(path: &Path) -> Option<StateFile> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn write_state_file(path: &Path, state: &StateFile) -> anyhow::Result<()> {
    let contents = serde_json::to_vec_pretty(state)?;
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &contents)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Thread-safe store of per-instance enabled/disabled state persisted as a
/// JSON file inside the config directory. Without a config dir the store is a
/// pure in-memory no-op (state transitions still succeed).
#[derive(Debug)]
pub struct InstanceStateStore {
    path: Option<PathBuf>,
    inner: Mutex<StateFile>,
}

impl InstanceStateStore {
    pub fn new(config_dir: Option<&Path>) -> Self {
        let path = config_dir.map(|dir| dir.join(STATE_FILE_NAME));
        let inner = Mutex::new(
            path.as_deref()
                .and_then(read_state_file)
                .unwrap_or_default(),
        );
        Self { path, inner }
    }

    /// Convenience constructor for an in-memory no-op store (no config dir).
    /// Used by hosts that never persist instance state (FFI, ohrs, mini, GUI,
    /// tests). Equivalent to `InstanceStateStore::new(None)`.
    pub fn in_memory() -> Self {
        Self::new(None)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether the instance should run at startup. Unknown instances default
    /// to enabled so that pre-existing config files keep their official
    /// "scan and start everything" behavior.
    pub fn is_enabled(&self, instance_id: &uuid::Uuid) -> bool {
        self.inner
            .lock()
            .unwrap()
            .instances
            .get(&instance_id.to_string())
            .map(|state| state.enabled)
            .unwrap_or(true)
    }

    pub fn set_enabled(&self, instance_id: uuid::Uuid, enabled: bool) -> anyhow::Result<()> {
        let mut state = self.inner.lock().unwrap();
        state
            .instances
            .insert(instance_id.to_string(), InstanceState { enabled });
        self.persist(&state)
    }

    pub fn remove(&self, instance_id: &uuid::Uuid) -> anyhow::Result<()> {
        let mut state = self.inner.lock().unwrap();
        state.instances.remove(&instance_id.to_string());
        self.persist(&state)
    }

    /// All instance ids recorded as disabled (stopped) in the store.
    pub fn disabled_instance_ids(&self) -> Vec<uuid::Uuid> {
        self.inner
            .lock()
            .unwrap()
            .instances
            .iter()
            .filter(|(_, state)| !state.enabled)
            .filter_map(|(id, _)| uuid::Uuid::parse_str(id).ok())
            .collect()
    }

    fn persist(&self, state: &StateFile) -> anyhow::Result<()> {
        if let Some(path) = self.path.as_deref() {
            write_state_file(path, state)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("easytier-state-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unknown_instances_default_to_enabled() {
        let store = InstanceStateStore::new(None);
        assert!(store.is_enabled(&uuid::Uuid::new_v4()));
        assert!(store.disabled_instance_ids().is_empty());
    }

    #[test]
    fn state_persists_across_store_recreation() {
        let dir = temp_dir();
        let instance_id = uuid::Uuid::new_v4();
        {
            let store = InstanceStateStore::new(Some(&dir));
            store.set_enabled(instance_id, false).unwrap();
            assert!(!store.is_enabled(&instance_id));
        }
        let store = InstanceStateStore::new(Some(&dir));
        assert!(!store.is_enabled(&instance_id));
        assert_eq!(store.disabled_instance_ids(), vec![instance_id]);
        assert!(dir.join(STATE_FILE_NAME).is_file());
    }

    #[test]
    fn remove_clears_state_and_then_defaults_to_enabled() {
        let dir = temp_dir();
        let instance_id = uuid::Uuid::new_v4();
        let store = InstanceStateStore::new(Some(&dir));
        store.set_enabled(instance_id, false).unwrap();
        assert!(!store.is_enabled(&instance_id));
        store.remove(&instance_id).unwrap();
        assert!(store.is_enabled(&instance_id));
        assert!(store.disabled_instance_ids().is_empty());
    }

    #[test]
    fn memory_only_store_still_tracks_state() {
        let instance_id = uuid::Uuid::new_v4();
        let store = InstanceStateStore::new(None);
        store.set_enabled(instance_id, false).unwrap();
        assert!(!store.is_enabled(&instance_id));
        store.set_enabled(instance_id, true).unwrap();
        assert!(store.is_enabled(&instance_id));
    }

    #[test]
    fn corrupt_state_file_falls_back_to_defaults() {
        let dir = temp_dir();
        std::fs::write(dir.join(STATE_FILE_NAME), "not json").unwrap();
        let store = InstanceStateStore::new(Some(&dir));
        assert!(store.is_enabled(&uuid::Uuid::new_v4()));
    }
}
