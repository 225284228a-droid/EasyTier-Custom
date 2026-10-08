//! Disk authority and hot-patch persistence shared by all remote read paths.

use std::{path::Path, sync::Arc};

use crate::{
    config::toml::{ConfigLoader as _, TomlConfig},
    instance::{
        CoreInstance, CoreInstanceHost,
        manager::{InstanceFactory, InstanceManager},
    },
    management::{
        ConfigFileStorage,
        full::{merge_active_changes, persisted_raw_hash, refresh_local_configs},
    },
};

/// Caller holds the process coordinator. Reads original bytes, never the
/// environment-expanded active configuration of a protected instance.
pub(crate) async fn read_original_config<F, H>(
    manager: &Arc<InstanceManager<F>>,
    storage: &dyn ConfigFileStorage,
    id: uuid::Uuid,
) -> anyhow::Result<Option<(TomlConfig, String)>>
where
    F: InstanceFactory<Instance = CoreInstance<H>>,
    H: CoreInstanceHost,
{
    if let Some(control) = manager.config_control(id)
        && control.is_read_only()
    {
        anyhow::bail!("configuration is protected");
    }
    if storage.supports_catalog() && manager.config_dir().is_some() {
        let state = manager
            .local_config_catalog()
            .state_store()
            .ok_or_else(|| anyhow::anyhow!("local configuration observation is unavailable"))?;
        let snapshot = refresh_local_configs(manager, storage, &state).await?;
        let mut entries = snapshot
            .entries
            .into_iter()
            .filter(|entry| entry.inst_id.map(uuid::Uuid::from) == Some(id));
        let entry = entries
            .next()
            .ok_or_else(|| anyhow::anyhow!("persisted configuration is unavailable"))?;
        if entries.next().is_some() || entry.status != "ready" {
            anyhow::bail!("configuration is protected or unavailable");
        }
        let raw = entry
            .persisted_toml
            .ok_or_else(|| anyhow::anyhow!("configuration is protected"))?;
        let config = TomlConfig::new_from_str(&raw)
            .map_err(|_| anyhow::anyhow!("invalid persisted configuration"))?;
        return Ok(Some((config, raw)));
    }
    let control = manager
        .config_control(id)
        .or_else(|| manager.managed_config_control(id));
    let Some(control) = control else {
        return Ok(None);
    };
    if control.is_read_only() {
        anyhow::bail!("configuration is protected");
    }
    let Some(path) = control.path.as_deref() else {
        return Ok(None);
    };
    let Some(raw) = storage.read(path).await? else {
        anyhow::bail!("persisted configuration is unavailable");
    };
    let directory = manager
        .config_dir()
        .map(|directory| directory.as_path())
        .or_else(|| path.parent())
        .unwrap_or(Path::new(""));
    if storage
        .inspect_raw_config(path, &raw, directory)
        .await?
        .is_read_only()
    {
        anyhow::bail!("configuration is protected");
    }
    let raw =
        String::from_utf8(raw).map_err(|_| anyhow::anyhow!("invalid persisted configuration"))?;
    let config = TomlConfig::new_from_str(&raw)
        .map_err(|_| anyhow::anyhow!("invalid persisted configuration"))?;
    if config.get_id() != id {
        anyhow::bail!("configuration identity changed");
    }
    Ok(Some((config, raw)))
}

/// Caller owns process then instance locks. Return the exact committed byte
/// hash so a later host-failure rollback cannot overwrite a manual edit.
pub(super) async fn persist_active_changes<F, H>(
    manager: &Arc<InstanceManager<F>>,
    storage: &dyn ConfigFileStorage,
    id: uuid::Uuid,
    candidate: &TomlConfig,
) -> anyhow::Result<Option<String>>
where
    F: InstanceFactory<Instance = CoreInstance<H>>,
    H: CoreInstanceHost,
{
    let control = manager
        .config_control(id)
        .ok_or_else(|| anyhow::anyhow!("configuration control is unavailable"))?;
    if control.is_read_only() {
        anyhow::bail!("configuration is protected");
    }
    let Some(path) = control.path.as_deref() else {
        return Ok(None);
    };
    let original = read_original_config(manager, storage, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("persisted configuration is unavailable"))?
        .1;
    let before = manager
        .config(id)
        .ok_or_else(|| anyhow::anyhow!("active configuration is unavailable"))?
        .dump();
    let merged = merge_active_changes(&original, &before, &candidate.dump())?;
    let current = storage
        .read(path)
        .await?
        .ok_or_else(|| anyhow::anyhow!("persisted configuration is unavailable"))?;
    if current != original.as_bytes() {
        anyhow::bail!("local_config_revision_conflict");
    }
    storage.write(path, merged.as_bytes()).await?;
    manager.local_config_catalog().request_refresh();
    Ok(Some(persisted_raw_hash(merged.as_bytes())))
}

pub(super) async fn rollback_active_changes<F, H>(
    manager: &Arc<InstanceManager<F>>,
    storage: &dyn ConfigFileStorage,
    id: uuid::Uuid,
    previous: &TomlConfig,
    written: &TomlConfig,
    expected_raw_hash: Option<&str>,
) -> anyhow::Result<()>
where
    F: InstanceFactory<Instance = CoreInstance<H>>,
    H: CoreInstanceHost,
{
    let control = manager
        .config_control(id)
        .ok_or_else(|| anyhow::anyhow!("configuration control is unavailable"))?;
    let Some(path) = control.path.as_deref() else {
        return Ok(());
    };
    let original = read_original_config(manager, storage, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("persisted configuration is unavailable"))?
        .1;
    if expected_raw_hash != Some(persisted_raw_hash(original.as_bytes()).as_str()) {
        anyhow::bail!("local_config_revision_conflict");
    }
    let merged = merge_active_changes(&original, &written.dump(), &previous.dump())?;
    if storage.read(path).await?.as_deref() != Some(original.as_bytes()) {
        anyhow::bail!("local_config_revision_conflict");
    }
    storage.write(path, merged.as_bytes()).await?;
    manager.local_config_catalog().request_refresh();
    Ok(())
}
