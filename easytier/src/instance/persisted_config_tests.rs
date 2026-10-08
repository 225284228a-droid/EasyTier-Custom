//! Two independent consoles share the same node-owned coordinator/catalog.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use easytier_core::management::{
    ConfigFileControl, ConfigFilePermission, ConfigFileStorage, InstanceStateStore,
    ProcessManagementRpc,
};
use easytier_proto::{
    api::manage::{
        DeleteNetworkInstanceRequest, NetworkConfig, NetworkingMethod, ObserveConfigsRequest,
        PatchPersistedConfigRequest, PersistedConfigApplyMode, PersistedConfigMutationStatus,
        PersistedConfigService, RemoveNetworkInstanceConfigRequest, RunNetworkInstanceRequest,
        SetNetworkInstanceEnabledRequest, WebClientService,
    },
    rpc_types::controller::BaseController,
};
use tokio::sync::Notify;

use super::factory::{NativeInstanceManager, native_instance_manager_with_config_dir};
use crate::{
    common::config::{ConfigLoader as _, TomlConfigLoader},
    web_client::DefaultHooks,
};

#[derive(Default)]
struct MemoryFiles {
    files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    readonly: Mutex<Vec<PathBuf>>,
    duplicate_on_load: Mutex<Option<PathBuf>>,
    writes: AtomicUsize,
    block_next_write: AtomicBool,
    write_started: Notify,
    release_write: Notify,
}

#[async_trait::async_trait]
impl ConfigFileStorage for MemoryFiles {
    fn supports_catalog(&self) -> bool {
        true
    }

    async fn list_configs(&self, directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .keys()
            .filter(|path| path.parent() == Some(directory))
            .cloned()
            .collect())
    }

    async fn inspect(&self, path: &Path) -> ConfigFileControl {
        ConfigFileControl::new(
            Some(path.to_owned()),
            ConfigFilePermission::from(
                if self.readonly.lock().unwrap().contains(&path.to_owned()) {
                    ConfigFilePermission::READ_ONLY | ConfigFilePermission::NO_DELETE
                } else {
                    0
                },
            ),
        )
    }

    async fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.files.lock().unwrap().get(path).cloned())
    }

    async fn load_config(
        &self,
        path: &Path,
        _config_dir: Option<&Path>,
    ) -> anyhow::Result<Option<(TomlConfigLoader, ConfigFileControl)>> {
        let Some(contents) = self.read(path).await? else {
            return Ok(None);
        };
        let config = TomlConfigLoader::new_from_str_with_source(
            &path.display().to_string(),
            std::str::from_utf8(&contents)?,
        )?;
        let control = self.inspect(path).await;
        if let Some(alias) = self.duplicate_on_load.lock().unwrap().take() {
            self.files.lock().unwrap().insert(alias, contents);
        }
        Ok(Some((config, control)))
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> anyhow::Result<()> {
        self.writes.fetch_add(1, Ordering::AcqRel);
        if self.block_next_write.swap(false, Ordering::AcqRel) {
            self.write_started.notify_one();
            self.release_write.notified().await;
        }
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), contents.to_vec());
        Ok(())
    }

    async fn remove(&self, path: &Path) -> anyhow::Result<()> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }
}

fn fixture() -> (
    Arc<NativeInstanceManager>,
    Arc<MemoryFiles>,
    ProcessManagementRpc<super::factory::NativeInstanceFactory>,
) {
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        PathBuf::from("virtual-configs"),
    )));
    let files = Arc::new(MemoryFiles::default());
    let rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(DefaultHooks),
        files.clone(),
        Arc::new(InstanceStateStore::in_memory()),
    );
    (manager, files, rpc)
}

fn path(id: uuid::Uuid) -> PathBuf {
    PathBuf::from("virtual-configs").join(format!("{id}.toml"))
}

fn initial(id: uuid::Uuid, hostname: &str) -> String {
    format!(
        "# keep this comment\ninstance_id = '{id}'\nhostname = '{hostname}'\nnetns = 'keep-netns'\n[network_identity]\nnetwork_name = 'mesh'\nnetwork_secret = 'secret'\n[flags]\nno_tun = true\n"
    )
}

async fn observe(
    rpc: &ProcessManagementRpc<super::factory::NativeInstanceFactory>,
) -> easytier_proto::api::manage::ObserveConfigsResponse {
    PersistedConfigService::observe_configs(
        rpc,
        BaseController::default(),
        ObserveConfigsRequest {},
    )
    .await
    .unwrap()
}

fn patch(
    id: uuid::Uuid,
    revision: String,
    hostname: &str,
    mode: PersistedConfigApplyMode,
) -> PatchPersistedConfigRequest {
    PatchPersistedConfigRequest {
        inst_id: Some(id.into()),
        expected_revision: revision,
        config: Some(NetworkConfig {
            hostname: Some(hostname.into()),
            ..Default::default()
        }),
        field_mask: vec!["hostname".into()],
        apply_mode: mode as i32,
    }
}

fn apply_persisted(id: uuid::Uuid, revision: String) -> PatchPersistedConfigRequest {
    PatchPersistedConfigRequest {
        inst_id: Some(id.into()),
        expected_revision: revision,
        config: Some(NetworkConfig::default()),
        field_mask: Vec::new(),
        apply_mode: PersistedConfigApplyMode::SaveAndApply as i32,
    }
}

async fn start_saved_instance(
    manager: &NativeInstanceManager,
    rpc: &ProcessManagementRpc<super::factory::NativeInstanceFactory>,
    id: uuid::Uuid,
) {
    use easytier_core::instance::CoreInstanceState;
    WebClientService::set_network_instance_enabled(
        rpc,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: true,
            expected_revision: None,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while manager.instance(id).unwrap().state() != CoreInstanceState::Running {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn two_consoles_cannot_overwrite_the_same_base_revision() {
    let (_, files, first) = fixture();
    let second = first.clone();
    let id = uuid::Uuid::new_v4();
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "base").into_bytes());
    let baseline = observe(&first).await;
    let revision = baseline.entries[0].revision.clone();
    let (one, two) = tokio::join!(
        PersistedConfigService::patch_config(
            &first,
            BaseController::default(),
            patch(
                id,
                revision.clone(),
                "one",
                PersistedConfigApplyMode::PersistOnly
            )
        ),
        PersistedConfigService::patch_config(
            &second,
            BaseController::default(),
            patch(id, revision, "two", PersistedConfigApplyMode::PersistOnly)
        ),
    );
    let mut statuses = vec![one.unwrap().status, two.unwrap().status];
    statuses.sort();
    assert_eq!(
        statuses,
        vec![
            PersistedConfigMutationStatus::Success as i32,
            PersistedConfigMutationStatus::Conflict as i32
        ]
    );
    let raw = String::from_utf8(files.read(&path(id)).await.unwrap().unwrap()).unwrap();
    assert!(raw.contains("# keep this comment"));
    assert!(raw.contains("netns = 'keep-netns'"));
    assert_eq!(observe(&first).await, observe(&second).await);
}

#[tokio::test]
async fn manual_edit_is_observed_and_stale_write_is_rejected() {
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "before").into_bytes());
    let baseline = observe(&rpc).await;
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "manual").into_bytes());
    let observed = observe(&rpc).await;
    assert!(observed.catalog_generation > baseline.catalog_generation);
    assert_ne!(observed.entries[0].revision, baseline.entries[0].revision);
    assert_eq!(
        observed.entries[0]
            .config
            .as_ref()
            .unwrap()
            .hostname
            .as_deref(),
        Some("manual")
    );
    assert!(
        manager.instance_ids().is_empty(),
        "observation cannot start an instance"
    );
    let rejected = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(
            id,
            baseline.entries[0].revision.clone(),
            "stale",
            PersistedConfigApplyMode::SaveAndApply,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        rejected.status,
        PersistedConfigMutationStatus::Conflict as i32
    );
    assert!(manager.instance_ids().is_empty());
}

#[tokio::test]
async fn request_cancellation_does_not_release_a_pending_write() {
    let (_, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "base").into_bytes());
    let revision = observe(&rpc).await.entries[0].revision.clone();
    files.block_next_write.store(true, Ordering::Release);
    let first_rpc = rpc.clone();
    let first_request = patch(
        id,
        revision.clone(),
        "first",
        PersistedConfigApplyMode::PersistOnly,
    );
    let caller = tokio::spawn(async move {
        PersistedConfigService::patch_config(&first_rpc, BaseController::default(), first_request)
            .await
    });
    files.write_started.notified().await;
    caller.abort();
    let second_rpc = rpc.clone();
    let second = tokio::spawn(async move {
        PersistedConfigService::patch_config(
            &second_rpc,
            BaseController::default(),
            patch(
                id,
                revision,
                "second",
                PersistedConfigApplyMode::PersistOnly,
            ),
        )
        .await
    });
    tokio::task::yield_now().await;
    assert!(
        !second.is_finished(),
        "the cancelled caller's actual operation still owns the coordinator"
    );
    files.release_write.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        result.status,
        PersistedConfigMutationStatus::Conflict as i32
    );
    assert_eq!(
        observe(&rpc).await.entries[0]
            .config
            .as_ref()
            .unwrap()
            .hostname
            .as_deref(),
        Some("first")
    );
}

#[tokio::test]
async fn protected_and_invalid_files_do_not_expose_configuration() {
    let (_, files, rpc) = fixture();
    let protected = uuid::Uuid::new_v4();
    let invalid = uuid::Uuid::new_v4();
    files.files.lock().unwrap().insert(
        path(protected),
        initial(protected, "protected").into_bytes(),
    );
    files.readonly.lock().unwrap().push(path(protected));
    files
        .files
        .lock()
        .unwrap()
        .insert(path(invalid), b"network_secret = [ broken-secret".to_vec());
    let snapshot = observe(&rpc).await;
    assert_eq!(snapshot.entries.len(), 2);
    for entry in &snapshot.entries {
        assert!(entry.persisted_toml.is_none());
        assert!(entry.config.is_none());
    }
    let response = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(
            invalid,
            String::new(),
            "replace",
            PersistedConfigApplyMode::PersistOnly,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        response.status,
        PersistedConfigMutationStatus::Conflict as i32
    );
    assert_eq!(
        files.read(&path(invalid)).await.unwrap().unwrap(),
        b"network_secret = [ broken-secret"
    );
}

#[tokio::test]
async fn persist_only_keeps_the_running_instance_and_reports_pending_apply() {
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let mut raw = initial(id, "active");
    // Native namespace adapters are not available in this portable test.
    raw = raw.replace("netns = 'keep-netns'\n", "");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), raw.clone().into_bytes());
    let config = TomlConfigLoader::new_from_str(&raw).unwrap();
    let mut network = easytier_core::config::api::network_config_from_toml(&config);
    network.networking_method = Some(NetworkingMethod::Standalone as i32);
    WebClientService::run_network_instance(
        &rpc,
        BaseController::default(),
        RunNetworkInstanceRequest {
            inst_id: Some(id.into()),
            config: Some(network),
            overwrite: true,
            source: 0,
        },
    )
    .await
    .unwrap();
    let running = manager.instance(id).unwrap();
    let baseline = observe(&rpc).await;
    assert!(
        !baseline.entries[0].pending_apply,
        "formatting differences alone are not pending changes"
    );
    let result = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(
            id,
            baseline.entries[0].revision.clone(),
            "persisted",
            PersistedConfigApplyMode::PersistOnly,
        ),
    )
    .await
    .unwrap();
    assert_eq!(result.status, PersistedConfigMutationStatus::Success as i32);
    assert!(result.entry.unwrap().pending_apply);
    assert!(Arc::ptr_eq(&running, &manager.instance(id).unwrap()));
    assert_eq!(manager.config(id).unwrap().get_hostname(), "active");
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn save_and_apply_of_a_disabled_config_does_not_enable_it() {
    use easytier_proto::api::manage::SetNetworkInstanceEnabledRequest;
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "stopped").into_bytes());
    let before = observe(&rpc).await;
    WebClientService::set_network_instance_enabled(
        &rpc,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: false,
            expected_revision: Some(before.entries[0].revision.clone()),
        },
    )
    .await
    .unwrap();
    let before = observe(&rpc).await;
    let response = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(
            id,
            before.entries[0].revision.clone(),
            "saved",
            PersistedConfigApplyMode::SaveAndApply,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        response.status,
        PersistedConfigMutationStatus::Success as i32
    );
    let entry = response.entry.unwrap();
    assert!(!entry.running);
    assert!(!entry.enabled);
    assert!(manager.instance_ids().is_empty());
}

#[tokio::test]
async fn save_and_apply_replaces_running_instance_and_keeps_original_non_form_fields() {
    use easytier_core::instance::CoreInstanceState;
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let raw = initial(id, "active").replace("netns = 'keep-netns'", "unmodeled = 'keep'");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), raw.clone().into_bytes());
    WebClientService::run_network_instance(
        &rpc,
        BaseController::default(),
        RunNetworkInstanceRequest {
            inst_id: Some(id.into()),
            config: Some(easytier_core::config::api::network_config_from_toml(
                &TomlConfigLoader::new_from_str(&raw).unwrap(),
            )),
            overwrite: true,
            source: 0,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while manager.instance(id).unwrap().state() != CoreInstanceState::Running {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let running = manager.instance(id).unwrap();
    let baseline = observe(&rpc).await;
    let result = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(
            id,
            baseline.entries[0].revision.clone(),
            "applied",
            PersistedConfigApplyMode::SaveAndApply,
        ),
    )
    .await
    .unwrap();
    assert_eq!(result.status, PersistedConfigMutationStatus::Success as i32);
    assert!(!Arc::ptr_eq(&running, &manager.instance(id).unwrap()));
    assert_eq!(manager.config(id).unwrap().get_hostname(), "applied");
    assert!(!result.entry.unwrap().pending_apply);
    let raw = String::from_utf8(files.read(&path(id)).await.unwrap().unwrap()).unwrap();
    assert!(raw.starts_with("# keep this comment"));
    assert!(raw.contains("unmodeled = 'keep'"));
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn apply_only_restarts_from_current_toml_without_writing_or_projecting_fields() {
    use easytier_core::config::toml::ConfigSource;
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let raw = initial(id, "active").replace("netns = 'keep-netns'", "unmodeled = 'keep'");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), raw.into_bytes());
    start_saved_instance(&manager, &rpc, id).await;
    assert!(
        manager
            .management_capabilities()
            .iter()
            .any(|cap| { cap == "management:persisted-config-apply-v1" })
    );
    let before = observe(&rpc).await;
    let saved = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(
            id,
            before.entries[0].revision.clone(),
            "pending",
            PersistedConfigApplyMode::PersistOnly,
        ),
    )
    .await
    .unwrap();
    assert_eq!(saved.status, PersistedConfigMutationStatus::Success as i32);
    assert!(saved.entry.as_ref().unwrap().pending_apply);
    let pending = String::from_utf8(files.read(&path(id)).await.unwrap().unwrap())
        .unwrap()
        .replace("no_tun = true", "no_tun = true\nmtu = 1337");
    let pending = format!("{pending}\n[source]\nsource = 'web'\n");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), pending.clone().into_bytes());
    let current = observe(&rpc).await.entries.remove(0);
    let running = manager.instance(id).unwrap();
    let writes = files.writes.load(Ordering::Acquire);
    let stale = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        apply_persisted(id, saved.entry.unwrap().revision),
    )
    .await
    .unwrap();
    assert_eq!(stale.status, PersistedConfigMutationStatus::Conflict as i32);
    assert!(Arc::ptr_eq(&running, &manager.instance(id).unwrap()));
    let result = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        apply_persisted(id, current.revision.clone()),
    )
    .await
    .unwrap();
    assert_eq!(result.status, PersistedConfigMutationStatus::Success as i32);
    let applied = result.entry.unwrap();
    assert!(!applied.pending_apply);
    assert_ne!(applied.revision, current.revision);
    assert!(!Arc::ptr_eq(&running, &manager.instance(id).unwrap()));
    assert_eq!(manager.config(id).unwrap().get_hostname(), "pending");
    assert_eq!(manager.config(id).unwrap().get_flags().mtu, 1337);
    assert_eq!(manager.config_source(id), Some(ConfigSource::Web));
    assert_eq!(files.writes.load(Ordering::Acquire), writes);
    assert_eq!(
        files.read(&path(id)).await.unwrap().unwrap(),
        pending.as_bytes()
    );
    assert!(pending.contains("# keep this comment"));
    assert!(pending.contains("unmodeled = 'keep'"));
    let implicit_user = pending.replace("\n[source]\nsource = 'web'\n", "");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), implicit_user.clone().into_bytes());
    let user_pending = observe(&rpc).await.entries.remove(0);
    assert!(user_pending.pending_apply);
    let result = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        apply_persisted(id, user_pending.revision),
    )
    .await
    .unwrap();
    assert_eq!(result.status, PersistedConfigMutationStatus::Success as i32);
    assert!(!result.entry.unwrap().pending_apply);
    assert_eq!(manager.config_source(id), Some(ConfigSource::User));
    assert_eq!(files.writes.load(Ordering::Acquire), writes);
    assert_eq!(
        files.read(&path(id)).await.unwrap().unwrap(),
        implicit_user.as_bytes()
    );
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn empty_mask_only_applies_existing_configs_and_never_enables_stopped_instances() {
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let raw = initial(id, "stopped").into_bytes();
    files.files.lock().unwrap().insert(path(id), raw.clone());
    WebClientService::set_network_instance_enabled(
        &rpc,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: false,
            expected_revision: None,
        },
    )
    .await
    .unwrap();
    let before = observe(&rpc).await;
    let base_request = apply_persisted(id, before.entries[0].revision.clone());
    let result =
        PersistedConfigService::patch_config(&rpc, BaseController::default(), base_request.clone())
            .await
            .unwrap();
    assert_eq!(result.status, PersistedConfigMutationStatus::Success as i32);
    assert_eq!(result.entry.as_ref(), before.entries.first());
    let mut persist_only = base_request.clone();
    persist_only.apply_mode = PersistedConfigApplyMode::PersistOnly as i32;
    let mut unmasked_edits = base_request;
    unmasked_edits.config.as_mut().unwrap().hostname = Some("ignored-edit".into());
    for request in [
        persist_only,
        unmasked_edits,
        apply_persisted(uuid::Uuid::new_v4(), String::new()),
    ] {
        let rejected =
            PersistedConfigService::patch_config(&rpc, BaseController::default(), request)
                .await
                .unwrap();
        assert_eq!(
            rejected.status,
            PersistedConfigMutationStatus::Invalid as i32
        );
    }
    assert_eq!(observe(&rpc).await, before);
    assert!(manager.instance_ids().is_empty());
    assert_eq!(files.files.lock().unwrap().len(), 1);
    assert_eq!(files.read(&path(id)).await.unwrap().unwrap(), raw);
    assert_eq!(files.writes.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn failed_apply_only_restores_active_config_and_keeps_pending_file_untouched() {
    use easytier_core::management::InstanceMutationHooks;
    struct FailAfterRun;
    #[async_trait::async_trait]
    impl InstanceMutationHooks for FailAfterRun {
        async fn post_run_network_instance(&self, _: &uuid::Uuid) -> Result<(), String> {
            Err("host start failed".into())
        }
    }
    let (manager, files, first) = fixture();
    let id = uuid::Uuid::new_v4();
    let original = initial(id, "active").replace("netns = 'keep-netns'\n", "");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), original.clone().into_bytes());
    start_saved_instance(&manager, &first, id).await;
    let pending = original.replace("hostname = 'active'", "hostname = 'pending'");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), pending.clone().into_bytes());
    let before = observe(&first).await.entries.remove(0);
    assert!(before.pending_apply);
    let applying = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(FailAfterRun),
        files.clone(),
        Arc::new(InstanceStateStore::in_memory()),
    );
    let result = PersistedConfigService::patch_config(
        &applying,
        BaseController::default(),
        apply_persisted(id, before.revision),
    )
    .await
    .unwrap();
    assert_eq!(
        result.status,
        PersistedConfigMutationStatus::ApplyFailed as i32
    );
    let entry = result.entry.unwrap();
    assert!(entry.pending_apply && entry.running && entry.enabled);
    assert_eq!(manager.config(id).unwrap().get_hostname(), "active");
    assert_eq!(
        files.read(&path(id)).await.unwrap().unwrap(),
        pending.as_bytes()
    );
    assert_eq!(files.writes.load(Ordering::Acquire), 0);
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn legacy_hot_patch_preserves_pending_disk_fields_and_comments() {
    use easytier_core::{instance::CoreInstanceState, management::InstanceManagementRpc};
    use easytier_proto::api::config::{ConfigRpc, InstanceConfigPatch, PatchConfigRequest};
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let raw = initial(id, "active").replace("netns = 'keep-netns'\n", "");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), raw.clone().into_bytes());
    let network = easytier_core::config::api::network_config_from_toml(
        &TomlConfigLoader::new_from_str(&raw).unwrap(),
    );
    WebClientService::run_network_instance(
        &rpc,
        BaseController::default(),
        RunNetworkInstanceRequest {
            inst_id: Some(id.into()),
            config: Some(network),
            overwrite: true,
            source: 0,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while manager.instance(id).unwrap().state() != CoreInstanceState::Running {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut pending = String::from_utf8(files.read(&path(id)).await.unwrap().unwrap()).unwrap();
    pending = pending
        .replace("hostname = \"active\"", "hostname = \"pending\"")
        .replace("hostname = 'active'", "hostname = 'pending'");
    pending.insert_str(0, "# manual comment kept\n");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), pending.into_bytes());
    let hot_rpc = InstanceManagementRpc::new_with_config_storage(manager.clone(), files.clone());
    ConfigRpc::patch_config(
        &hot_rpc,
        BaseController::default(),
        PatchConfigRequest {
            instance: None,
            patch: Some(InstanceConfigPatch {
                disable_relay_data: Some(true),
                ..Default::default()
            }),
        },
    )
    .await
    .unwrap();
    let final_raw = String::from_utf8(files.read(&path(id)).await.unwrap().unwrap()).unwrap();
    assert!(final_raw.starts_with("# manual comment kept"));
    assert_eq!(
        TomlConfigLoader::new_from_str(&final_raw)
            .unwrap()
            .get_hostname(),
        "pending"
    );
    assert!(
        TomlConfigLoader::new_from_str(&final_raw)
            .unwrap()
            .get_flags()
            .disable_relay_data
    );
    assert_eq!(manager.config(id).unwrap().get_hostname(), "active");
    assert!(observe(&rpc).await.entries[0].pending_apply);
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn clearing_stopped_file_permissions_restores_editability() {
    let (_, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "base").into_bytes());
    let baseline = observe(&rpc).await;
    files.readonly.lock().unwrap().push(path(id));
    let restricted = observe(&rpc).await;
    assert_eq!(restricted.entries[0].status, "protected");
    assert_ne!(restricted.entries[0].revision, baseline.entries[0].revision);
    files.readonly.lock().unwrap().clear();
    let restored = observe(&rpc).await;
    assert_eq!(restored.entries[0].status, "ready");
    assert!(restored.entries[0].persisted_toml.is_some());
}

#[tokio::test(start_paused = true)]
async fn periodic_observation_refreshes_without_a_read_request_or_starting_instances() {
    let (manager, files, _rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "initial").into_bytes());
    let wait_for_hostname = |expected: &'static str| {
        let manager = manager.clone();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let snapshot = manager.local_config_catalog().snapshot();
                    if snapshot.entries.iter().any(|entry| {
                        entry
                            .config
                            .as_ref()
                            .and_then(|config| config.hostname.as_deref())
                            == Some(expected)
                    }) {
                        return snapshot;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the node observer must refresh without ObserveConfigs calls")
        }
    };
    let before = wait_for_hostname("initial").await;
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "manual").into_bytes());
    tokio::time::sleep(std::time::Duration::from_secs(29)).await;
    assert_eq!(
        manager.local_config_catalog().snapshot(),
        before,
        "manual edits must not trigger a periodic scan before 30 seconds"
    );
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let after = manager.local_config_catalog().snapshot();
    assert_eq!(
        after.entries[0]
            .config
            .as_ref()
            .unwrap()
            .hostname
            .as_deref(),
        Some("manual"),
        "the periodic scan must publish the edit after 30 seconds"
    );
    assert!(after.catalog_generation > before.catalog_generation);
    assert_ne!(after.entries[0].revision, before.entries[0].revision);

    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "notified").into_bytes());
    let notified_at = tokio::time::Instant::now();
    manager.local_config_catalog().request_refresh();
    let notified = wait_for_hostname("notified").await;
    assert!(notified.catalog_generation > after.catalog_generation);
    assert!(
        notified_at.elapsed() < std::time::Duration::from_secs(1),
        "management notifications must refresh without waiting for the next scan"
    );
    assert!(manager.instance_ids().is_empty());
}

#[tokio::test]
async fn lifecycle_mutations_reject_revisions_from_before_enabled_state_changed() {
    use easytier_proto::api::manage::{
        RemoveNetworkInstanceConfigRequest, SetNetworkInstanceEnabledRequest,
    };
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "base").into_bytes());
    let revision = observe(&rpc).await.entries[0].revision.clone();
    WebClientService::set_network_instance_enabled(
        &rpc,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: false,
            expected_revision: Some(revision.clone()),
        },
    )
    .await
    .unwrap();
    let stale_enable = WebClientService::set_network_instance_enabled(
        &rpc,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: true,
            expected_revision: Some(revision.clone()),
        },
    )
    .await
    .unwrap_err();
    assert!(
        stale_enable
            .to_string()
            .contains("local_config_revision_conflict")
    );
    let stale_remove = WebClientService::remove_network_instance_config(
        &rpc,
        BaseController::default(),
        RemoveNetworkInstanceConfigRequest {
            inst_ids: vec![id.into()],
            expected_revision: Some(revision),
        },
    )
    .await
    .unwrap_err();
    assert!(
        stale_remove
            .to_string()
            .contains("local_config_revision_conflict")
    );
    assert!(files.read(&path(id)).await.unwrap().is_some());
    assert!(manager.instance_ids().is_empty());
    assert!(!observe(&rpc).await.entries[0].enabled);
}

#[tokio::test]
async fn all_remote_read_interfaces_use_original_disk_and_enforce_current_permissions() {
    use easytier_core::management::InstanceManagementRpc;
    use easytier_proto::api::{
        config::{ConfigRpc, GetConfigRequest},
        manage::GetNetworkInstanceConfigRequest,
    };
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let raw = initial(id, "active").replace("netns = 'keep-netns'\n", "");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), raw.clone().into_bytes());
    WebClientService::run_network_instance(
        &rpc,
        BaseController::default(),
        RunNetworkInstanceRequest {
            inst_id: Some(id.into()),
            config: Some(easytier_core::config::api::network_config_from_toml(
                &TomlConfigLoader::new_from_str(&raw).unwrap(),
            )),
            overwrite: true,
            source: 0,
        },
    )
    .await
    .unwrap();
    let pending = initial(id, "pending").replace("netns = 'keep-netns'\n", "");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), pending.clone().into_bytes());
    let hot_rpc = InstanceManagementRpc::new_with_config_storage(manager.clone(), files.clone());
    let original = ConfigRpc::get_config(
        &hot_rpc,
        BaseController::default(),
        GetConfigRequest { instance: None },
    )
    .await
    .unwrap();
    assert_eq!(original.toml_config, pending);
    assert_eq!(
        original.config.unwrap().hostname.as_deref(),
        Some("pending")
    );
    let web_original = WebClientService::get_network_instance_config(
        &rpc,
        BaseController::default(),
        GetNetworkInstanceConfigRequest {
            inst_id: Some(id.into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        web_original.config.unwrap().hostname.as_deref(),
        Some("pending")
    );
    assert_eq!(manager.config(id).unwrap().get_hostname(), "active");

    // No ObserveConfigs call precedes either check: direct RPC cannot reuse
    // cached permissions to expose a file that just became protected.
    files.readonly.lock().unwrap().push(path(id));
    assert!(
        ConfigRpc::get_config(
            &hot_rpc,
            BaseController::default(),
            GetConfigRequest { instance: None },
        )
        .await
        .is_err()
    );
    assert!(
        WebClientService::get_network_instance_config(
            &rpc,
            BaseController::default(),
            GetNetworkInstanceConfigRequest {
                inst_id: Some(id.into())
            },
        )
        .await
        .is_err()
    );
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn duplicate_identities_are_hidden_and_cannot_be_written_by_legacy_clients() {
    use easytier_proto::api::manage::{
        GetNetworkInstanceConfigRequest, SaveNetworkInstanceConfigRequest,
    };
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let alias = uuid::Uuid::new_v4();
    let raw = initial(id, "base").replace("netns = 'keep-netns'\n", "");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), raw.clone().into_bytes());
    files
        .files
        .lock()
        .unwrap()
        .insert(path(alias), raw.clone().into_bytes());
    let snapshot = observe(&rpc).await;
    assert_eq!(snapshot.entries.len(), 2);
    assert!(snapshot.entries.iter().all(|entry| {
        entry.status == "identity_conflict"
            && entry.config.is_none()
            && entry.persisted_toml.is_none()
    }));
    let mut config = easytier_core::config::api::network_config_from_toml(
        &TomlConfigLoader::new_from_str(&raw).unwrap(),
    );
    config.hostname = Some("legacy-edit".into());
    let rejected_save = WebClientService::save_network_instance_config(
        &rpc,
        BaseController::default(),
        SaveNetworkInstanceConfigRequest {
            inst_id: Some(id.into()),
            config: Some(config.clone()),
            source: 0,
        },
    )
    .await
    .unwrap_err();
    assert!(
        rejected_save
            .to_string()
            .contains("configuration identity is ambiguous")
    );
    let rejected_run = WebClientService::run_network_instance(
        &rpc,
        BaseController::default(),
        RunNetworkInstanceRequest {
            inst_id: Some(id.into()),
            config: Some(config),
            overwrite: true,
            source: 0,
        },
    )
    .await
    .unwrap_err();
    assert!(
        rejected_run
            .to_string()
            .contains("configuration identity is ambiguous")
    );
    assert!(
        WebClientService::get_network_instance_config(
            &rpc,
            BaseController::default(),
            GetNetworkInstanceConfigRequest {
                inst_id: Some(id.into())
            },
        )
        .await
        .is_err()
    );
    assert_eq!(
        files.read(&path(id)).await.unwrap().unwrap(),
        raw.as_bytes()
    );
    assert_eq!(
        files.read(&path(alias)).await.unwrap().unwrap(),
        raw.as_bytes()
    );
    assert!(manager.instance_ids().is_empty());
}

#[tokio::test]
async fn legacy_lifecycle_preserves_ambiguous_ids_and_completes_other_batch_members() {
    for running in [false, true] {
        let (manager, files, rpc) = fixture();
        let id = uuid::Uuid::new_v4();
        let alias = uuid::Uuid::new_v4();
        let deleted = uuid::Uuid::new_v4();
        let removed = uuid::Uuid::new_v4();
        let raw = initial(id, "base").replace("netns = 'keep-netns'\n", "");
        files
            .files
            .lock()
            .unwrap()
            .insert(path(id), raw.clone().into_bytes());
        if running {
            start_saved_instance(&manager, &rpc, id).await;
        } else {
            WebClientService::set_network_instance_enabled(
                &rpc,
                BaseController::default(),
                SetNetworkInstanceEnabledRequest {
                    inst_id: Some(id.into()),
                    enabled: false,
                    expected_revision: None,
                },
            )
            .await
            .unwrap();
        }
        let instance = manager.instance(id);
        {
            let mut contents = files.files.lock().unwrap();
            contents.insert(path(alias), raw.clone().into_bytes());
            contents.insert(path(deleted), initial(deleted, "delete-me").into_bytes());
            contents.insert(path(removed), initial(removed, "remove-me").into_bytes());
        }
        for enabled in [true, false] {
            let error = WebClientService::set_network_instance_enabled(
                &rpc,
                BaseController::default(),
                SetNetworkInstanceEnabledRequest {
                    inst_id: Some(id.into()),
                    enabled,
                    expected_revision: None,
                },
            )
            .await
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("configuration identity is ambiguous")
            );
        }
        let result = WebClientService::delete_network_instance(
            &rpc,
            BaseController::default(),
            DeleteNetworkInstanceRequest {
                inst_ids: vec![id.into(), deleted.into()],
                expected_revision: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.remain_inst_ids, vec![id.into()]);
        assert!(files.read(&path(deleted)).await.unwrap().is_none());
        let result = WebClientService::remove_network_instance_config(
            &rpc,
            BaseController::default(),
            RemoveNetworkInstanceConfigRequest {
                inst_ids: vec![id.into(), removed.into()],
                expected_revision: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.remain_inst_ids, vec![id.into()]);
        assert!(files.read(&path(removed)).await.unwrap().is_none());
        assert_eq!(
            files.read(&path(id)).await.unwrap().unwrap(),
            raw.as_bytes()
        );
        assert_eq!(
            files.read(&path(alias)).await.unwrap().unwrap(),
            raw.as_bytes()
        );
        assert!(
            observe(&rpc)
                .await
                .entries
                .iter()
                .all(|entry| { entry.status == "identity_conflict" && entry.enabled == running })
        );
        if let Some(instance) = instance {
            assert!(Arc::ptr_eq(&instance, &manager.instance(id).unwrap()));
            manager.delete_network_instances([id]).await.unwrap();
        } else {
            assert!(manager.instance_ids().is_empty());
        }
    }
}

#[tokio::test]
async fn legacy_lifecycle_rechecks_duplicates_created_during_hooks_and_config_loading() {
    use easytier_core::management::InstanceMutationHooks;
    struct DuplicateOnHook {
        files: Arc<MemoryFiles>,
        alias: PathBuf,
        on_start: bool,
    }
    impl DuplicateOnHook {
        fn duplicate(&self, id: uuid::Uuid) {
            let mut files = self.files.files.lock().unwrap();
            let contents = files.get(&path(id)).unwrap().clone();
            files.insert(self.alias.clone(), contents);
        }
    }
    #[async_trait::async_trait]
    impl InstanceMutationHooks for DuplicateOnHook {
        async fn pre_run_network_instance(&self, config: &TomlConfigLoader) -> Result<(), String> {
            if self.on_start {
                self.duplicate(config.get_id());
            }
            Ok(())
        }
        async fn post_stop_network_instances(&self, ids: &[uuid::Uuid]) -> Result<(), String> {
            if !self.on_start {
                for id in ids {
                    self.duplicate(*id);
                }
            }
            Ok(())
        }
    }
    for operation in [
        "enable",
        "delete",
        "remove",
        "disable-on-load",
        "delete-on-load",
    ] {
        let (manager, files, first) = fixture();
        let id = uuid::Uuid::new_v4();
        let alias = path(uuid::Uuid::new_v4());
        let raw = initial(id, "base").replace("netns = 'keep-netns'\n", "");
        files
            .files
            .lock()
            .unwrap()
            .insert(path(id), raw.clone().into_bytes());
        if matches!(operation, "delete" | "disable-on-load" | "delete-on-load") {
            start_saved_instance(&manager, &first, id).await;
        }
        let active_before = manager.instance(id);
        let guarded = ProcessManagementRpc::new(
            manager.clone(),
            Arc::new(DuplicateOnHook {
                files: files.clone(),
                alias: alias.clone(),
                on_start: operation == "enable",
            }),
            files.clone(),
            Arc::new(InstanceStateStore::in_memory()),
        );
        match operation {
            "enable" => {
                let error = WebClientService::set_network_instance_enabled(
                    &guarded,
                    BaseController::default(),
                    SetNetworkInstanceEnabledRequest {
                        inst_id: Some(id.into()),
                        enabled: true,
                        expected_revision: None,
                    },
                )
                .await
                .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("configuration identity is ambiguous")
                );
            }
            "delete" => {
                let result = WebClientService::delete_network_instance(
                    &guarded,
                    BaseController::default(),
                    DeleteNetworkInstanceRequest {
                        inst_ids: vec![id.into()],
                        expected_revision: None,
                    },
                )
                .await
                .unwrap();
                assert_eq!(result.remain_inst_ids, vec![id.into()]);
            }
            "remove" => {
                *files.duplicate_on_load.lock().unwrap() = Some(alias.clone());
                let result = WebClientService::remove_network_instance_config(
                    &guarded,
                    BaseController::default(),
                    RemoveNetworkInstanceConfigRequest {
                        inst_ids: vec![id.into()],
                        expected_revision: None,
                    },
                )
                .await
                .unwrap();
                assert_eq!(result.remain_inst_ids, vec![id.into()]);
            }
            "disable-on-load" => {
                *files.duplicate_on_load.lock().unwrap() = Some(alias.clone());
                let error = WebClientService::set_network_instance_enabled(
                    &guarded,
                    BaseController::default(),
                    SetNetworkInstanceEnabledRequest {
                        inst_id: Some(id.into()),
                        enabled: false,
                        expected_revision: None,
                    },
                )
                .await
                .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("configuration identity is ambiguous")
                );
            }
            "delete-on-load" => {
                *files.duplicate_on_load.lock().unwrap() = Some(alias.clone());
                let result = WebClientService::delete_network_instance(
                    &guarded,
                    BaseController::default(),
                    DeleteNetworkInstanceRequest {
                        inst_ids: vec![id.into()],
                        expected_revision: None,
                    },
                )
                .await
                .unwrap();
                assert_eq!(result.remain_inst_ids, vec![id.into()]);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            files.read(&path(id)).await.unwrap().unwrap(),
            raw.as_bytes()
        );
        assert_eq!(files.read(&alias).await.unwrap().unwrap(), raw.as_bytes());
        if matches!(operation, "disable-on-load" | "delete-on-load") {
            assert!(Arc::ptr_eq(
                &active_before.unwrap(),
                &manager.instance(id).unwrap()
            ));
            assert!(
                observe(&first)
                    .await
                    .entries
                    .iter()
                    .all(|entry| entry.enabled)
            );
            manager.delete_network_instances([id]).await.unwrap();
        } else {
            assert!(manager.instance(id).is_none());
        }
        assert_eq!(files.writes.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn apply_capability_requires_observation_and_legacy_runtime_without_directory_still_works() {
    use easytier_core::management::UnsupportedConfigFileStorage;
    for directory in [None, Some(PathBuf::from("virtual-configs"))] {
        let manager = Arc::new(native_instance_manager_with_config_dir(directory.clone()));
        let storage: Arc<dyn ConfigFileStorage> = if directory.is_none() {
            Arc::new(MemoryFiles::default())
        } else {
            Arc::new(UnsupportedConfigFileStorage)
        };
        let rpc = ProcessManagementRpc::new(
            manager.clone(),
            Arc::new(DefaultHooks),
            storage,
            Arc::new(InstanceStateStore::in_memory()),
        );
        assert!(!manager.management_capabilities().iter().any(|cap| {
            cap == "management:persisted-config-apply-v1"
                || cap == "management:persisted-config-revision-v1"
        }));
        if directory.is_some() {
            continue;
        }
        let id = uuid::Uuid::new_v4();
        let config = TomlConfigLoader::new_from_str(
            &initial(id, "legacy-managed").replace("netns = 'keep-netns'\n", ""),
        )
        .unwrap();
        WebClientService::run_network_instance(
            &rpc,
            BaseController::default(),
            RunNetworkInstanceRequest {
                inst_id: Some(id.into()),
                config: Some(easytier_core::config::api::network_config_from_toml(
                    &config,
                )),
                overwrite: true,
                source: 0,
            },
        )
        .await
        .unwrap();
        WebClientService::set_network_instance_enabled(
            &rpc,
            BaseController::default(),
            SetNetworkInstanceEnabledRequest {
                inst_id: Some(id.into()),
                enabled: false,
                expected_revision: None,
            },
        )
        .await
        .unwrap();
        let removed = WebClientService::delete_network_instance(
            &rpc,
            BaseController::default(),
            DeleteNetworkInstanceRequest {
                inst_ids: vec![id.into()],
                expected_revision: None,
            },
        )
        .await
        .unwrap();
        assert!(removed.remain_inst_ids.is_empty());
        assert!(manager.instance_ids().is_empty());
    }
}

#[tokio::test]
async fn local_read_without_registered_storage_does_not_fall_back_to_active_config() {
    use easytier_core::management::InstanceManagementRpc;
    use easytier_proto::api::config::{ConfigRpc, GetConfigRequest};
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        PathBuf::from("virtual-configs"),
    )));
    let id = uuid::Uuid::new_v4();
    let config = TomlConfigLoader::new_from_str(
        &initial(id, "active").replace("netns = 'keep-netns'\n", ""),
    )
    .unwrap();
    manager
        .run_network_instance(
            config,
            ConfigFileControl::new(Some(path(id)), ConfigFilePermission::default()),
        )
        .unwrap();
    assert!(!manager.local_config_catalog().supports_revision());
    let rpc = InstanceManagementRpc::new(manager.clone());
    let rejected = ConfigRpc::get_config(
        &rpc,
        BaseController::default(),
        GetConfigRequest { instance: None },
    )
    .await
    .unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("persisted configuration is unavailable")
    );
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn failed_apply_does_not_rollback_a_file_that_just_became_protected() {
    use easytier_core::{instance::CoreInstanceState, management::InstanceMutationHooks};
    struct ProtectAfterRun(Arc<MemoryFiles>);
    #[async_trait::async_trait]
    impl InstanceMutationHooks for ProtectAfterRun {
        async fn post_run_network_instance(&self, id: &uuid::Uuid) -> Result<(), String> {
            self.0.readonly.lock().unwrap().push(path(*id));
            Err("host start failed after permissions changed".into())
        }
    }
    let (manager, files, first) = fixture();
    let id = uuid::Uuid::new_v4();
    let raw = initial(id, "active").replace("netns = 'keep-netns'\n", "");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), raw.clone().into_bytes());
    WebClientService::run_network_instance(
        &first,
        BaseController::default(),
        RunNetworkInstanceRequest {
            inst_id: Some(id.into()),
            config: Some(easytier_core::config::api::network_config_from_toml(
                &TomlConfigLoader::new_from_str(&raw).unwrap(),
            )),
            overwrite: true,
            source: 0,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while manager.instance(id).unwrap().state() != CoreInstanceState::Running {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let applying = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(ProtectAfterRun(files.clone())),
        files.clone(),
        Arc::new(InstanceStateStore::in_memory()),
    );
    let revision = observe(&first).await.entries[0].revision.clone();
    let result = PersistedConfigService::patch_config(
        &applying,
        BaseController::default(),
        patch(
            id,
            revision,
            "written",
            PersistedConfigApplyMode::SaveAndApply,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        result.status,
        PersistedConfigMutationStatus::ApplyFailed as i32
    );
    let entry = result.entry.unwrap();
    assert_eq!(entry.status, "protected");
    assert!(entry.config.is_none());
    assert!(entry.persisted_toml.is_none());
    let persisted = String::from_utf8(files.read(&path(id)).await.unwrap().unwrap()).unwrap();
    assert_eq!(
        TomlConfigLoader::new_from_str(&persisted)
            .unwrap()
            .get_hostname(),
        "written"
    );
    assert_eq!(manager.config(id).unwrap().get_hostname(), "active");
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn empty_revision_creates_only_absent_configs_and_keeps_them_disabled() {
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let created = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        PatchPersistedConfigRequest {
            inst_id: Some(id.into()),
            expected_revision: String::new(),
            config: Some(NetworkConfig {
                hostname: Some("new-node-config".into()),
                network_name: Some("mesh".into()),
                network_secret: Some("secret".into()),
                networking_method: Some(NetworkingMethod::Standalone as i32),
                no_tun: Some(true),
                ..Default::default()
            }),
            field_mask: vec![
                "hostname".into(),
                "network_name".into(),
                "network_secret".into(),
                "networking_method".into(),
                "no_tun".into(),
            ],
            apply_mode: PersistedConfigApplyMode::SaveAndApply as i32,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        created.status,
        PersistedConfigMutationStatus::Success as i32
    );
    let entry = created.entry.unwrap();
    assert!(!entry.enabled);
    assert!(!entry.running);
    assert!(!entry.revision.is_empty());
    assert_eq!(
        entry.config.unwrap().hostname.as_deref(),
        Some("new-node-config")
    );
    assert!(manager.instance_ids().is_empty());
    let raw = files.read(&path(id)).await.unwrap().unwrap();
    let retry = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(
            id,
            String::new(),
            "overwrite",
            PersistedConfigApplyMode::PersistOnly,
        ),
    )
    .await
    .unwrap();
    assert_eq!(retry.status, PersistedConfigMutationStatus::Conflict as i32);
    assert_eq!(files.read(&path(id)).await.unwrap().unwrap(), raw);
}

#[tokio::test]
async fn cas_enable_rejects_a_file_edited_by_pre_run_hook() {
    use easytier_core::management::InstanceMutationHooks;
    use easytier_proto::api::manage::SetNetworkInstanceEnabledRequest;

    struct EditBeforeRun(Arc<MemoryFiles>);
    #[async_trait::async_trait]
    impl InstanceMutationHooks for EditBeforeRun {
        async fn pre_run_network_instance(&self, config: &TomlConfigLoader) -> Result<(), String> {
            self.0.files.lock().unwrap().insert(
                path(config.get_id()),
                initial(config.get_id(), "manual").into_bytes(),
            );
            Ok(())
        }
    }
    let (manager, files, first) = fixture();
    let id = uuid::Uuid::new_v4();
    files.files.lock().unwrap().insert(
        path(id),
        initial(id, "before")
            .replace("netns = 'keep-netns'\n", "")
            .into_bytes(),
    );
    WebClientService::set_network_instance_enabled(
        &first,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: false,
            expected_revision: None,
        },
    )
    .await
    .unwrap();
    let revision = observe(&first).await.entries[0].revision.clone();
    let enabling = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(EditBeforeRun(files.clone())),
        files.clone(),
        Arc::new(InstanceStateStore::in_memory()),
    );
    let error = WebClientService::set_network_instance_enabled(
        &enabling,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: true,
            expected_revision: Some(revision),
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("local_config_revision_conflict"));
    assert!(manager.instance(id).is_none());
    let refreshed = observe(&first).await;
    assert!(!refreshed.entries[0].enabled);
    assert_eq!(
        refreshed.entries[0]
            .config
            .as_ref()
            .unwrap()
            .hostname
            .as_deref(),
        Some("manual")
    );
}

#[tokio::test]
async fn cas_delete_preserves_files_changed_by_post_stop_hook() {
    use easytier_core::{instance::CoreInstanceState, management::InstanceMutationHooks};
    use easytier_proto::api::manage::DeleteNetworkInstanceRequest;

    struct EditAfterStop {
        files: Arc<MemoryFiles>,
        protect_only: bool,
    }
    #[async_trait::async_trait]
    impl InstanceMutationHooks for EditAfterStop {
        async fn post_stop_network_instances(&self, ids: &[uuid::Uuid]) -> Result<(), String> {
            for id in ids {
                if self.protect_only {
                    self.files.readonly.lock().unwrap().push(path(*id));
                } else {
                    self.files
                        .files
                        .lock()
                        .unwrap()
                        .insert(path(*id), initial(*id, "manual").into_bytes());
                }
            }
            Ok(())
        }
    }
    for protect_only in [false, true] {
        let (manager, files, first) = fixture();
        let id = uuid::Uuid::new_v4();
        let original = initial(id, "active").replace("netns = 'keep-netns'\n", "");
        files
            .files
            .lock()
            .unwrap()
            .insert(path(id), original.clone().into_bytes());
        WebClientService::run_network_instance(
            &first,
            BaseController::default(),
            RunNetworkInstanceRequest {
                inst_id: Some(id.into()),
                config: Some(easytier_core::config::api::network_config_from_toml(
                    &TomlConfigLoader::new_from_str(&original).unwrap(),
                )),
                overwrite: true,
                source: 0,
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while manager.instance(id).unwrap().state() != CoreInstanceState::Running {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let persisted_before = files.read(&path(id)).await.unwrap().unwrap();
        let revision = observe(&first).await.entries[0].revision.clone();
        let deleting = ProcessManagementRpc::new(
            manager.clone(),
            Arc::new(EditAfterStop {
                files: files.clone(),
                protect_only,
            }),
            files.clone(),
            Arc::new(InstanceStateStore::in_memory()),
        );
        let error = WebClientService::delete_network_instance(
            &deleting,
            BaseController::default(),
            DeleteNetworkInstanceRequest {
                inst_ids: vec![id.into()],
                expected_revision: Some(revision),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("local_config_revision_conflict"));
        assert!(manager.instance(id).is_none());
        let persisted = files.read(&path(id)).await.unwrap().unwrap();
        if protect_only {
            assert_eq!(persisted, persisted_before);
            assert_eq!(observe(&first).await.entries[0].status, "protected");
        } else {
            assert_eq!(persisted, initial(id, "manual").as_bytes());
            assert_eq!(
                observe(&first).await.entries[0]
                    .config
                    .as_ref()
                    .unwrap()
                    .hostname
                    .as_deref(),
                Some("manual")
            );
        }
    }
}
