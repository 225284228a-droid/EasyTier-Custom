use super::*;
use crate::{
    config::toml::TomlConfig,
    instance::manager::InstanceFactory,
    management::{
        ActiveInstanceForStart, ConfigFileControl, ConfigFilePermission, ConfigFileStorage,
        InstanceManager, InstanceMutationHooks, InstanceStateStore, ProcessManagement,
    },
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

struct Factory(Arc<CoreProcessRuntime>, Arc<AtomicBool>);
impl InstanceFactory for Factory {
    type Instance = CoreInstance<TestHost>;
    type CreateContext = ();
    type Error = anyhow::Error;
    fn create(&self, config: TomlConfig, (): ()) -> anyhow::Result<Arc<Self::Instance>> {
        if self.1.load(Ordering::Relaxed) {
            anyhow::bail!("injected factory failure");
        }
        let (sink, _) = tokio::sync::mpsc::channel(16);
        CoreInstance::from_toml(
            config,
            adapters_with_process_runtime(None, Arc::new(sink), self.0.clone()),
        )
    }
}

#[derive(Default)]
struct Files {
    contents: Mutex<HashMap<PathBuf, Vec<u8>>>,
    permissions: Mutex<HashMap<PathBuf, u8>>,
    failed_remove: Mutex<Vec<PathBuf>>,
    failed_write: AtomicBool,
}
#[async_trait]
impl ConfigFileStorage for Files {
    async fn inspect(&self, path: &Path) -> ConfigFileControl {
        ConfigFileControl::new(
            Some(path.to_owned()),
            ConfigFilePermission::from(*self.permissions.lock().unwrap().get(path).unwrap_or(&0)),
        )
    }
    async fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.contents.lock().unwrap().get(path).cloned())
    }
    async fn write(&self, path: &Path, contents: &[u8]) -> anyhow::Result<()> {
        if self.failed_write.load(Ordering::Relaxed) {
            anyhow::bail!("injected write failure");
        }
        self.contents
            .lock()
            .unwrap()
            .insert(path.to_owned(), contents.to_vec());
        Ok(())
    }
    async fn remove(&self, path: &Path) -> anyhow::Result<()> {
        if self
            .failed_remove
            .lock()
            .unwrap()
            .contains(&path.to_path_buf())
        {
            anyhow::bail!("injected remove failure");
        }
        self.contents.lock().unwrap().remove(path);
        Ok(())
    }
}
#[derive(Default)]
struct Hooks {
    events: Mutex<Vec<&'static str>>,
    reject_post: AtomicBool,
    replace_tun: AtomicBool,
    extra_stop: Mutex<Option<uuid::Uuid>>,
    break_state_path: Mutex<Option<PathBuf>>,
    reject_stop_once: AtomicBool,
}
#[async_trait]
impl InstanceMutationHooks for Hooks {
    async fn prepare_start(
        &self,
        config: &TomlConfig,
        active: &[ActiveInstanceForStart],
    ) -> Result<Vec<uuid::Uuid>, String> {
        self.events.lock().unwrap().push("prepare");
        let mut ids = if self.replace_tun.load(Ordering::Relaxed) {
            active
                .iter()
                .filter(|item| item.instance_id != config.get_id() && !item.no_tun)
                .map(|item| item.instance_id)
                .collect()
        } else {
            Vec::new()
        };
        ids.extend(*self.extra_stop.lock().unwrap());
        Ok(ids)
    }
    async fn pre_run_network_instance(&self, _: &TomlConfig) -> Result<(), String> {
        self.events.lock().unwrap().push("pre");
        Ok(())
    }
    async fn post_run_network_instance(&self, _: &uuid::Uuid) -> Result<(), String> {
        self.events.lock().unwrap().push("post");
        if let Some(path) = self.break_state_path.lock().unwrap().as_ref() {
            std::fs::remove_file(path).unwrap();
            std::fs::create_dir(path).unwrap();
        }
        if self.reject_post.load(Ordering::Relaxed) {
            Err("injected post failure".into())
        } else {
            Ok(())
        }
    }
    async fn post_stop_network_instances(&self, _: &[uuid::Uuid]) -> Result<(), String> {
        self.events.lock().unwrap().push("stop");
        if self.reject_stop_once.swap(false, Ordering::Relaxed) {
            return Err("injected stop failure".into());
        }
        Ok(())
    }
    async fn post_remove_network_instances(&self, _: &[uuid::Uuid]) -> Result<(), String> {
        self.events.lock().unwrap().push("remove");
        Ok(())
    }
}
struct Fixture {
    manager: Arc<InstanceManager<Factory>>,
    files: Arc<Files>,
    hooks: Arc<Hooks>,
    state: Arc<InstanceStateStore>,
    process: ProcessManagement<Factory>,
    fail_create: Arc<AtomicBool>,
}
impl Fixture {
    fn new() -> Self {
        let fail_create = Arc::new(AtomicBool::new(false));
        let manager = Arc::new(
            InstanceManager::new(
                Factory(CoreProcessRuntime::new(), fail_create.clone()),
                None,
            )
            .with_config_path(Some(PathBuf::from("managed"))),
        );
        let files = Arc::new(Files::default());
        let hooks = Arc::new(Hooks::default());
        let state = Arc::new(InstanceStateStore::in_memory());
        let process =
            ProcessManagement::new(manager.clone(), hooks.clone(), files.clone(), state.clone());
        Self {
            manager,
            files,
            hooks,
            state,
            process,
            fail_create,
        }
    }
    fn saved(&self, name: &str, permission: u8) -> (uuid::Uuid, PathBuf) {
        let config = TomlConfig::default();
        config.set_inst_name(name.to_owned());
        config.set_listeners(Vec::new());
        let mut flags = config.get_flags();
        flags.no_tun = false;
        config.set_flags(flags);
        let id = config.get_id();
        let path = PathBuf::from("managed").join(format!("{name}.toml"));
        self.files
            .contents
            .lock()
            .unwrap()
            .insert(path.clone(), config.dump().into_bytes());
        self.files
            .permissions
            .lock()
            .unwrap()
            .insert(path.clone(), permission);
        self.manager
            .register_managed_config(
                id,
                ConfigFileControl::new(Some(path.clone()), ConfigFilePermission::from(permission)),
            )
            .unwrap();
        self.state.set_enabled(id, false).unwrap();
        (id, path)
    }
}

#[tokio::test]
async fn home_config_enable_disable_keeps_path_and_runs_hooks_once() {
    let f = Fixture::new();
    let (id, path) = f.saved("home", ConfigFilePermission::NO_DELETE);
    let original = f.files.contents.lock().unwrap().get(&path).unwrap().clone();
    f.process
        .set_network_instance_enabled(id, true)
        .await
        .unwrap();
    f.manager.unregister_managed_config(id);
    f.process
        .set_network_instance_enabled(id, false)
        .await
        .unwrap();
    assert!(!f.state.is_enabled(&id));
    assert!(f.manager.instance(id).is_none());
    assert_eq!(
        f.manager.managed_config_control(id).unwrap().path,
        Some(path.clone())
    );
    assert_eq!(f.files.contents.lock().unwrap().get(&path), Some(&original));
    assert!(
        f.process
            .get_network_instance_config_from_file(id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        *f.hooks.events.lock().unwrap(),
        vec!["prepare", "pre", "post", "stop"]
    );
}

#[tokio::test]
async fn complete_delete_handles_active_saved_and_protected_instances() {
    let f = Fixture::new();
    let (active, active_path) = f.saved("active", 0);
    let (saved, saved_path) = f.saved("saved", 0);
    let (protected, protected_path) = f.saved(
        "protected",
        ConfigFilePermission::READ_ONLY | ConfigFilePermission::NO_DELETE,
    );
    f.process
        .set_network_instance_enabled(active, true)
        .await
        .unwrap();
    let result = f
        .process
        .delete_network_instances(vec![active, saved, protected])
        .await
        .unwrap();
    assert!(result.removed_instance_ids.contains(&active));
    assert!(result.removed_instance_ids.contains(&saved));
    assert_eq!(result.remaining_instance_ids, vec![protected]);
    assert!(!f.state.is_enabled(&protected));
    let files = f.files.contents.lock().unwrap();
    assert!(!files.contains_key(&active_path));
    assert!(!files.contains_key(&saved_path));
    assert!(files.contains_key(&protected_path));
}

#[tokio::test]
async fn config_only_remove_preserves_protected_disabled_state() {
    let f = Fixture::new();
    for flag in [
        ConfigFilePermission::READ_ONLY,
        ConfigFilePermission::NO_DELETE,
    ] {
        let (id, path) = f.saved(&format!("protected-{flag}"), flag);
        assert!(
            f.process
                .remove_network_instance_configs(&[id])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!f.state.is_enabled(&id));
        assert!(f.files.contents.lock().unwrap().contains_key(&path));
    }
}

#[tokio::test]
async fn remove_failure_keeps_config_and_disabled_state() {
    let f = Fixture::new();
    let (id, path) = f.saved("failed-delete", 0);
    f.process
        .set_network_instance_enabled(id, true)
        .await
        .unwrap();
    f.files.failed_remove.lock().unwrap().push(path.clone());
    let result = f.process.delete_network_instances(vec![id]).await.unwrap();
    assert_eq!(result.remaining_instance_ids, vec![id]);
    assert!(result.removed_instance_ids.is_empty());
    assert!(!f.state.is_enabled(&id));
    assert!(f.manager.instance(id).is_none());
    assert!(f.files.contents.lock().unwrap().contains_key(&path));
    assert!(
        f.process
            .remove_network_instance_configs(&[id])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!f.state.is_enabled(&id));
}

#[tokio::test]
async fn enable_post_hook_failure_rolls_back_without_losing_config() {
    let f = Fixture::new();
    let (id, path) = f.saved("post-failure", 0);
    f.hooks.reject_post.store(true, Ordering::Relaxed);
    assert!(
        f.process
            .set_network_instance_enabled(id, true)
            .await
            .is_err()
    );
    assert!(f.manager.instance(id).is_none());
    assert!(!f.state.is_enabled(&id));
    assert!(f.files.contents.lock().unwrap().contains_key(&path));
}

#[tokio::test]
async fn unrecoverable_or_mismatched_config_cannot_be_disabled() {
    let f = Fixture::new();
    let (id, path) = f.saved("mismatch", 0);
    f.process
        .set_network_instance_enabled(id, true)
        .await
        .unwrap();
    f.files
        .contents
        .lock()
        .unwrap()
        .insert(path, b"instance_name = \"missing-id\"\n".to_vec());
    assert!(
        f.process
            .set_network_instance_enabled(id, false)
            .await
            .is_err()
    );
    assert!(f.manager.instance(id).is_some());
    assert!(f.state.is_enabled(&id));
    f.manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn start_plan_validates_all_stops_before_mutating() {
    let f = Fixture::new();
    f.hooks.replace_tun.store(true, Ordering::Relaxed);
    let (old, _) = f.saved("old-tun", 0);
    let (new, _) = f.saved("new-tun", 0);
    f.process
        .set_network_instance_enabled(old, true)
        .await
        .unwrap();
    *f.hooks.extra_stop.lock().unwrap() = Some(uuid::Uuid::new_v4());
    assert!(
        f.process
            .set_network_instance_enabled(new, true)
            .await
            .is_err()
    );
    assert!(f.manager.instance(old).is_some());
    assert!(f.state.is_enabled(&old));
    *f.hooks.extra_stop.lock().unwrap() = None;
    tokio::time::timeout(
        Duration::from_secs(1),
        f.process.set_network_instance_enabled(new, true),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(f.manager.instance(old).is_none());
    assert!(!f.state.is_enabled(&old));
    assert!(f.manager.instance(new).is_some());
    f.process
        .set_network_instance_enabled(new, false)
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_enable_keeps_one_tun_without_lock_reentry() {
    let f = Fixture::new();
    f.hooks.replace_tun.store(true, Ordering::Relaxed);
    let (first, _) = f.saved("first-tun", 0);
    let (second, _) = f.saved("second-tun", 0);
    tokio::time::timeout(Duration::from_secs(1), async {
        let (a, b) = tokio::join!(
            f.process.set_network_instance_enabled(first, true),
            f.process.set_network_instance_enabled(second, true)
        );
        a.unwrap();
        b.unwrap();
    })
    .await
    .unwrap();
    let active = f.manager.instance_ids();
    assert_eq!(active.len(), 1);
    f.process
        .set_network_instance_enabled(active[0], false)
        .await
        .unwrap();
}

#[tokio::test]
async fn enable_state_persistence_failure_stops_the_new_instance() {
    let f = Fixture::new();
    let (id, path) = f.saved("state-failure", 0);
    let dir = std::env::temp_dir().join(format!("easytier-enable-state-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = Arc::new(InstanceStateStore::new(Some(&dir)));
    state.set_enabled(id, false).unwrap();
    let state_path = state.path().unwrap();
    *f.hooks.break_state_path.lock().unwrap() = Some(state_path.to_path_buf());
    let process = ProcessManagement::new(
        f.manager.clone(),
        f.hooks.clone(),
        f.files.clone(),
        state.clone(),
    );
    assert!(
        process
            .set_network_instance_enabled(id, true)
            .await
            .is_err()
    );
    assert!(f.manager.instance(id).is_none());
    assert!(!state.is_enabled(&id));
    assert!(f.files.contents.lock().unwrap().contains_key(&path));
    assert_eq!(f.hooks.events.lock().unwrap().last(), Some(&"stop"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn duplicate_catalog_id_is_rejected_without_replacing_its_path() {
    let f = Fixture::new();
    let (id, path) = f.saved("first-path", 0);
    assert!(
        f.manager
            .register_managed_config(
                id,
                ConfigFileControl::new(
                    Some(PathBuf::from("managed/second-path.toml")),
                    ConfigFilePermission::default()
                )
            )
            .is_err()
    );
    assert_eq!(
        f.manager.managed_config_control(id).unwrap().path,
        Some(path)
    );
}

#[tokio::test]
async fn disable_ephemeral_instance_without_config_dir_stops_it() {
    let manager = Arc::new(InstanceManager::new(
        Factory(CoreProcessRuntime::new(), Arc::new(AtomicBool::new(false))),
        None,
    ));
    let config = TomlConfig::default();
    config.set_inst_name("ephemeral-instance".to_owned());
    config.set_listeners(Vec::new());
    let mut flags = config.get_flags();
    flags.no_tun = false;
    config.set_flags(flags);
    let id = config.get_id();
    manager
        .run_network_instance(
            config,
            ConfigFileControl::new(None, ConfigFilePermission::default()),
        )
        .unwrap();

    let process = ProcessManagement::new(
        manager.clone(),
        Arc::new(Hooks::default()),
        Arc::new(Files::default()),
        Arc::new(InstanceStateStore::in_memory()),
    );
    process
        .set_network_instance_enabled(id, false)
        .await
        .unwrap();

    assert!(manager.instance(id).is_none());
}

#[tokio::test]
async fn active_and_catalog_path_conflict_cannot_change_persisted_state() {
    let f = Fixture::new();
    let (id, path) = f.saved("catalog-home", 0);
    let bytes = f.files.contents.lock().unwrap().get(&path).unwrap().clone();
    let config = TomlConfig::new_from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
    let external = PathBuf::from("external.toml");
    f.files
        .contents
        .lock()
        .unwrap()
        .insert(external.clone(), bytes);
    f.manager
        .run_network_instance(
            config,
            ConfigFileControl::new(Some(external), ConfigFilePermission::default()),
        )
        .unwrap();
    assert!(
        f.manager
            .register_managed_config(
                id,
                ConfigFileControl::new(Some(path), ConfigFilePermission::default())
            )
            .is_err()
    );
    assert!(
        f.process
            .set_network_instance_enabled(id, false)
            .await
            .is_err()
    );
    assert!(
        f.process
            .set_network_instance_enabled(id, true)
            .await
            .is_err()
    );
    assert!(f.manager.instance(id).is_some());
    assert!(!f.state.is_enabled(&id));
    f.manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn failed_fresh_run_or_save_does_not_leave_ghost_disabled_state() {
    let f = Fixture::new();
    let config = TomlConfig::default();
    config.set_listeners(Vec::new());
    let id = config.get_id();
    *f.hooks.extra_stop.lock().unwrap() = Some(uuid::Uuid::new_v4());
    assert!(
        f.process
            .run_network_instance(config.clone(), None, false, None)
            .await
            .is_err()
    );
    assert_eq!(f.state.enabled_state(&id), None);
    *f.hooks.extra_stop.lock().unwrap() = None;
    f.fail_create.store(true, Ordering::Relaxed);
    assert!(
        f.process
            .run_network_instance(config.clone(), None, false, None)
            .await
            .is_err()
    );
    assert_eq!(f.state.enabled_state(&id), None);
    assert!(f.files.contents.lock().unwrap().is_empty());
    f.files.failed_write.store(true, Ordering::Relaxed);
    assert!(
        f.process
            .save_network_instance_config(config, None, None)
            .await
            .is_err()
    );
    assert_eq!(f.state.enabled_state(&id), None);
    assert!(f.state.disabled_instance_ids().is_empty());

    let manager = Arc::new(InstanceManager::new(
        Factory(CoreProcessRuntime::new(), Arc::new(AtomicBool::new(false))),
        None,
    ));
    let hooks = Arc::new(Hooks::default());
    hooks.reject_post.store(true, Ordering::Relaxed);
    let state = Arc::new(InstanceStateStore::in_memory());
    let files = Arc::new(Files::default());
    let process = ProcessManagement::new(manager.clone(), hooks, files, state.clone());
    let config = TomlConfig::default();
    config.set_listeners(Vec::new());
    let id = config.get_id();
    assert!(
        process
            .run_network_instance(config, None, false, None)
            .await
            .is_err()
    );
    assert_eq!(state.enabled_state(&id), None);
    assert!(state.disabled_instance_ids().is_empty());
    assert!(manager.instance_ids().is_empty());
}

#[tokio::test]
async fn stop_hook_failure_can_be_completed_by_idempotent_retry() {
    let f = Fixture::new();
    let (id, _) = f.saved("stop-retry", 0);
    f.process
        .set_network_instance_enabled(id, true)
        .await
        .unwrap();
    f.hooks.reject_stop_once.store(true, Ordering::Relaxed);
    assert!(
        f.process
            .set_network_instance_enabled(id, false)
            .await
            .is_err()
    );
    assert!(f.manager.instance(id).is_none());
    assert!(!f.state.is_enabled(&id));
    f.process
        .set_network_instance_enabled(id, false)
        .await
        .unwrap();
    assert_eq!(
        f.hooks
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| **event == "stop")
            .count(),
        2
    );
}

#[tokio::test]
async fn malformed_disabled_config_does_not_hide_other_metadata() {
    use easytier_proto::{
        api::manage::{ListNetworkInstanceMetaRequest, WebClientService},
        rpc_types::controller::BaseController,
    };
    let f = Fixture::new();
    let (good, _) = f.saved("valid-meta", 0);
    let (bad, path) = f.saved("malformed-meta", 0);
    f.files
        .contents
        .lock()
        .unwrap()
        .insert(path, b"dhcp = \"invalid\"\n".to_vec());
    let rpc = crate::management::ProcessManagementRpc::new(
        f.manager.clone(),
        f.hooks.clone(),
        f.files.clone(),
        f.state.clone(),
    );
    let response = rpc
        .list_network_instance_meta(
            BaseController::default(),
            ListNetworkInstanceMetaRequest {
                inst_ids: vec![good.into(), bad.into()],
            },
        )
        .await
        .unwrap();
    assert_eq!(response.metas.len(), 1);
    assert_eq!(response.metas[0].inst_id, Some(good.into()));
    assert!(
        f.process
            .get_network_instance_config_from_file(bad)
            .await
            .is_err()
    );
}
