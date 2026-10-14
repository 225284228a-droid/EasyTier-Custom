//! External TOML edits use the same process coordinator and Host lifecycle
//! hooks as RPC mutations. They never rewrite or project the user's document.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use super::{
    ConfigFileControl, ConfigFileStorage, LocalConfigCatalog, ProcessManagement,
    local_catalog::raw_hash,
};
use crate::{
    config::toml::{ConfigLoader as _, TomlConfig},
    instance::{
        CoreInstance, CoreInstanceHost,
        manager::{InstanceFactory, InstanceManager},
    },
};

#[derive(Clone)]
pub(super) struct ReconciledFile {
    hash: String,
    id: Option<uuid::Uuid>,
}

struct CoordinatedStorage {
    inner: Arc<dyn ConfigFileStorage>,
    catalog: Arc<LocalConfigCatalog>,
}

pub(crate) fn coordinated_storage<F: InstanceFactory>(
    manager: &Arc<InstanceManager<F>>,
    inner: Arc<dyn ConfigFileStorage>,
) -> Arc<dyn ConfigFileStorage> {
    Arc::new(CoordinatedStorage {
        inner,
        catalog: manager.local_config_catalog().clone(),
    })
}

#[async_trait::async_trait]
impl ConfigFileStorage for CoordinatedStorage {
    fn supports_catalog(&self) -> bool {
        self.inner.supports_catalog()
    }
    fn supports_live_reconcile(&self) -> bool {
        self.inner.supports_live_reconcile()
    }
    fn watch_configs(
        &self,
        directory: &Path,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> anyhow::Result<Option<Box<dyn Send>>> {
        self.inner.watch_configs(directory, changed)
    }
    async fn list_configs(&self, directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
        self.inner.list_configs(directory).await
    }
    async fn inspect_raw_config(
        &self,
        path: &Path,
        contents: &[u8],
        directory: &Path,
    ) -> anyhow::Result<ConfigFileControl> {
        self.inner
            .inspect_raw_config(path, contents, directory)
            .await
    }
    async fn inspect(&self, path: &Path) -> ConfigFileControl {
        self.inner.inspect(path).await
    }
    async fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        self.inner.read(path).await
    }
    async fn load_config(
        &self,
        path: &Path,
        directory: Option<&Path>,
    ) -> anyhow::Result<Option<(TomlConfig, ConfigFileControl)>> {
        self.inner.load_config(path, directory).await
    }
    async fn write(&self, path: &Path, contents: &[u8]) -> anyhow::Result<()> {
        self.inner.write(path, contents).await?;
        self.catalog.acknowledge_file(path, contents);
        Ok(())
    }
    async fn remove(&self, path: &Path) -> anyhow::Result<()> {
        self.inner.remove(path).await
    }
}

fn document_id(contents: &[u8]) -> Option<uuid::Uuid> {
    toml::from_str::<toml::Table>(std::str::from_utf8(contents).ok()?)
        .ok()?
        .get("instance_id")?
        .as_str()?
        .parse()
        .ok()
}

impl<F, H> ProcessManagement<F>
where
    F: InstanceFactory<Instance = CoreInstance<H>, CreateContext = ()>,
    F::Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
    H: CoreInstanceHost,
{
    /// Caller owns the coordinator. Complete the directory read before effects:
    /// a failed listing must never be interpreted as deleting every network.
    pub(super) async fn reconcile_local_files_locked(&self) -> anyhow::Result<()> {
        let Some(directory) = self.instances.config_dir() else {
            return Ok(());
        };
        let catalog = self.instances.local_config_catalog();
        let previous = catalog.reconciled_files.lock().unwrap().clone();
        let paths = self.storage.list_configs(directory).await?;
        let paths = paths
            .into_iter()
            .filter(|path| {
                path.parent() == Some(directory.as_path())
                    && path.extension() == Some(std::ffi::OsStr::new("toml"))
            })
            .collect::<HashSet<_>>();
        let mut next = previous.clone();
        let mut candidates = BTreeMap::new();
        let mut owners = BTreeMap::<uuid::Uuid, HashSet<PathBuf>>::new();
        let mut unidentified = false;
        for path in &paths {
            let raw = match self.storage.read(path).await {
                Ok(Some(raw)) => raw,
                // A concurrent replacement is retried on the next scan.
                Ok(None) => continue,
                Err(_) => {
                    unidentified = true;
                    continue;
                }
            };
            let hash = raw_hash(&raw);
            let raw_id = document_id(&raw).or_else(|| path.file_stem()?.to_str()?.parse().ok());
            if let Some(id) = raw_id {
                owners.entry(id).or_default().insert(path.clone());
            }
            let loaded = self.storage.load_config(path, Some(directory)).await;
            let (config, control) = match loaded {
                Ok(Some(loaded)) => loaded,
                _ => {
                    unidentified |= raw_id.is_none();
                    next.insert(path.clone(), ReconciledFile { hash, id: raw_id });
                    continue;
                }
            };
            // Legacy documents may omit a UUID. Reuse their existing runtime
            // identity rather than generating a different network every scan.
            let has_document_identity = std::str::from_utf8(&raw)
                .ok()
                .and_then(|text| toml::from_str::<toml::Table>(text).ok())
                .is_some_and(|table| table.contains_key("instance_id"));
            if !has_document_identity {
                let known_id = raw_id
                    .or_else(|| previous.get(path).and_then(|file| file.id))
                    .or_else(|| {
                        self.instances.managed_config_ids().into_iter().find(|id| {
                            self.instances
                                .managed_config_control(*id)
                                .is_some_and(|control| control.path.as_ref() == Some(path))
                        })
                    });
                if let Some(id) = known_id {
                    config.set_id(id);
                }
            }
            let id = config.get_id();
            super::local_catalog::reuse_implicit_identity(
                &raw,
                &config,
                self.instances.config(id).as_ref(),
            );
            owners.entry(id).or_default().insert(path.clone());
            if self.storage.read(path).await?.as_deref() != Some(raw.as_slice()) {
                continue;
            }
            candidates.insert(path.clone(), (config, control, hash));
        }
        // Remove disappeared files and old identities before starting their
        // replacements. Local deletion is authoritative even for read-only files.
        let registered = self
            .instances
            .managed_config_ids()
            .into_iter()
            .chain(self.instances.instance_ids())
            .collect::<HashSet<_>>();
        for id in registered {
            let control = self
                .instances
                .config_control(id)
                .or_else(|| self.instances.managed_config_control(id));
            let Some(path) = control.and_then(|control| control.path) else {
                continue;
            };
            if path.parent() != Some(directory.as_path()) {
                continue;
            }
            let replaced = candidates.get(&path).is_some_and(|(config, _, _)| {
                config.get_id() != id
                    && owners
                        .get(&config.get_id())
                        .is_some_and(|paths| paths.len() == 1)
            });
            if paths.contains(&path) && !replaced {
                continue;
            }
            if !paths.contains(&path)
                && owners.get(&id).is_some_and(|paths| paths.len() == 1)
                && let Some((new_path, (_, control, hash))) = candidates
                    .iter()
                    .find(|(_, (config, _, _))| config.get_id() == id)
                && self
                    .storage
                    .read(new_path)
                    .await?
                    .is_some_and(|raw| raw_hash(&raw) == *hash)
            {
                self.instances
                    .relocate_managed_config(id, &path, control.clone())?;
                catalog.forget_file(&path);
                next.remove(&path);
                continue;
            }
            // Recheck absence before stopping: rename/replace notifications may
            // have arrived while this scan was loading other files.
            if !replaced && self.storage.read(&path).await?.is_some() {
                continue;
            }
            self.instances.delete_network_instances([id]).await?;
            self.hooks
                .post_stop_network_instances(&[id])
                .await
                .map_err(anyhow::Error::msg)?;
            self.state_store.remove(&id)?;
            self.instances.unregister_managed_config(id);
            catalog.forget_file(&path);
            next.remove(&path);
        }
        for (path, (config, control, hash)) in candidates {
            let id = config.get_id();
            if owners.get(&id).is_some_and(|paths| paths.len() > 1) {
                continue;
            }
            let unchanged = previous
                .get(&path)
                .is_some_and(|file| file.hash == hash && file.id == Some(id));
            let coordinated = catalog.acknowledged_file(&path, &hash);
            if let Err(error) = self.instances.register_managed_config(id, control.clone()) {
                tracing::warn!(%error, %id, config_file = ?path, "skipping conflicting local configuration");
                continue;
            }
            let needs_start = self.instances.instance(id).is_none_or(|instance| {
                instance.state() == crate::instance::CoreInstanceState::Stopped
            });
            if (!unchanged || needs_start) && !coordinated && self.state_store.is_enabled(&id) {
                if let Err(error) = self
                    .apply_local_file_locked(config, control, &path, &hash)
                    .await
                {
                    // Leave the last applied baseline intact so the fallback
                    // scan retries recoverable failures without losing the edit.
                    tracing::warn!(%error, %id, config_file = ?path, "could not apply local configuration; keeping previous runtime");
                    continue;
                }
            }
            next.insert(path, ReconciledFile { hash, id: Some(id) });
        }
        next.retain(|path, _| paths.contains(path));
        catalog.forget_missing_files(&paths);
        // Reap stopped IDs as well; instance list RPCs must agree with the file
        // catalog. Ambiguous documents conservatively preserve unknown state.
        if !unidentified {
            let known_ids = owners.keys().copied().collect::<HashSet<_>>();
            for id in self.state_store.disabled_instance_ids() {
                if !known_ids.contains(&id) && self.instances.instance(id).is_none() {
                    self.state_store.remove(&id)?;
                }
            }
        }
        *catalog.reconciled_files.lock().unwrap() = next;
        Ok(())
    }

    async fn apply_local_file_locked(
        &self,
        config: TomlConfig,
        control: ConfigFileControl,
        path: &Path,
        hash: &str,
    ) -> anyhow::Result<()> {
        let id = config.get_id();
        // A comment/formatting-only edit must not interrupt an active network.
        if self
            .instances
            .config(id)
            .as_ref()
            .is_some_and(|active| super::local_catalog::configs_equivalent(active, &config))
            && self.instances.instance(id).is_some_and(|instance| {
                instance.state() != crate::instance::CoreInstanceState::Stopped
            })
        {
            return Ok(());
        }
        let old = self
            .instances
            .config(id)
            .zip(self.instances.config_control(id));
        self.prepare_start_locked(&config).await?;
        self.hooks
            .pre_run_network_instance(&config)
            .await
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            self.storage
                .read(path)
                .await?
                .as_ref()
                .is_some_and(|raw| raw_hash(raw) == hash),
            "local configuration changed while preparing to apply"
        );
        if old.is_some() {
            self.instances.delete_network_instances([id]).await?;
        }
        let outcome = async {
            self.instances.run_network_instance(config, control)?;
            self.hooks
                .post_run_network_instance(&id)
                .await
                .map_err(anyhow::Error::msg)?;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let instance = self
                        .instances
                        .instance(id)
                        .ok_or_else(|| anyhow::anyhow!("instance disappeared while starting"))?;
                    match instance.state() {
                        crate::instance::CoreInstanceState::Running => {
                            return Ok::<(), anyhow::Error>(());
                        }
                        crate::instance::CoreInstanceState::Stopped => {
                            anyhow::bail!("local instance failed to start")
                        }
                        _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                    }
                }
            })
            .await
            .map_err(|_| anyhow::anyhow!("timed out starting local instance"))??;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = outcome {
            self.instances.delete_network_instances([id]).await?;
            if let Some((config, control)) = old {
                self.instances.run_network_instance(config, control)?;
            }
            return Err(error);
        }
        tracing::info!(%id, config_file = ?path, "applied local configuration");
        Ok(())
    }
}
