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
        DeleteNetworkInstanceRequest, ListNetworkInstanceRequest, NetworkConfig, NetworkingMethod,
        ObserveConfigsRequest, PatchPersistedConfigRequest, PersistedConfigMutationStatus,
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

fn frontend_creation_fixtures() -> Vec<serde_json::Value> {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../easytier-web/frontend-lib/tests/fixtures/network-create-patches.json"
    )))
    .unwrap()
}

fn frontend_creation_request(fixture: &serde_json::Value) -> PatchPersistedConfigRequest {
    let mut request = fixture["request"].clone();
    let id: uuid::Uuid = request["inst_id"].as_str().unwrap().parse().unwrap();
    // Both the GUI command and Web REST adapter make this same UUID conversion.
    request["inst_id"] = serde_json::to_value(easytier_proto::common::Uuid::from(id)).unwrap();
    serde_json::from_value(request).unwrap()
}

#[tokio::test]
async fn frontend_creation_contract_round_trips_and_remains_disabled_after_reload() {
    use super::config_storage::NativeConfigFileStorage;
    use easytier_core::config::{api::network_config_from_toml, api_input::NetworkConfigExt as _};

    for fixture in frontend_creation_fixtures() {
        let request = frontend_creation_request(&fixture);
        let id = uuid::Uuid::from(request.inst_id.unwrap());
        let directory = tempfile::tempdir().unwrap();
        let storage = Arc::new(NativeConfigFileStorage::new(true));
        let state = Arc::new(InstanceStateStore::new(Some(directory.path())));
        let manager = Arc::new(native_instance_manager_with_config_dir(Some(
            directory.path().to_owned(),
        )));
        let rpc = ProcessManagementRpc::new(
            manager.clone(),
            Arc::new(DefaultHooks),
            storage.clone(),
            state.clone(),
        );
        let mut expected_values = request.config.clone().unwrap();
        expected_values.instance_id = Some(id.to_string());
        // The frontend compiles all three form methods to concrete peers.
        // Its sparse patch inherits Manual from the node's persisted projection.
        expected_values.networking_method = Some(NetworkingMethod::Manual as i32);
        let expected = network_config_from_toml(&expected_values.gen_config().unwrap());
        let response =
            PersistedConfigService::patch_config(&rpc, BaseController::default(), request.clone())
                .await
                .unwrap();
        assert_eq!(
            response.status,
            PersistedConfigMutationStatus::Success as i32,
            "{}: {:?}",
            fixture["name"],
            response.message
        );
        let entry = response.entry.unwrap();
        assert!(!entry.enabled && !entry.running);
        assert!(!entry.revision.is_empty());
        assert_eq!(
            entry.config.as_ref().unwrap(),
            &expected,
            "{}",
            fixture["name"]
        );
        assert!(!state.is_enabled(&id));
        assert!(manager.instance_ids().is_empty());
        let file = directory.path().join(format!("{id}.toml"));
        let contents = std::fs::read(&file).unwrap();
        let loaded =
            TomlConfigLoader::new_from_str(std::str::from_utf8(&contents).unwrap()).unwrap();
        assert_eq!(loaded.get_id(), id);
        assert_eq!(network_config_from_toml(&loaded), expected);
        let duplicate =
            PersistedConfigService::patch_config(&rpc, BaseController::default(), request)
                .await
                .unwrap();
        assert_eq!(
            duplicate.status,
            PersistedConfigMutationStatus::Conflict as i32
        );
        assert_eq!(std::fs::read(&file).unwrap(), contents);
        drop(rpc);
        drop(manager);
        drop(state);

        let reloaded = Arc::new(native_instance_manager_with_config_dir(Some(
            directory.path().to_owned(),
        )));
        let state = Arc::new(InstanceStateStore::new(Some(directory.path())));
        let rpc = ProcessManagementRpc::new(
            reloaded.clone(),
            Arc::new(DefaultHooks),
            storage,
            state.clone(),
        );
        let snapshot = observe(&rpc).await;
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.inst_id == Some(id.into()))
            .unwrap();
        assert!(!entry.enabled && !entry.running);
        assert!(!state.is_enabled(&id));
        assert!(reloaded.instance_ids().is_empty());
        assert_eq!(entry.config.as_ref().unwrap(), &expected);
    }
}

#[tokio::test]
async fn frontend_created_config_can_be_edited_enabled_stopped_and_reloaded() {
    use super::config_storage::NativeConfigFileStorage;

    let fixture = frontend_creation_fixtures()
        .into_iter()
        .find(|fixture| fixture["name"] == "standalone")
        .unwrap();
    let request = frontend_creation_request(&fixture);
    let id = uuid::Uuid::from(request.inst_id.unwrap());
    let directory = tempfile::tempdir().unwrap();
    let storage = Arc::new(NativeConfigFileStorage::new(true));
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        directory.path().to_owned(),
    )));
    let state = Arc::new(InstanceStateStore::new(Some(directory.path())));
    let rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(DefaultHooks),
        storage.clone(),
        state,
    );
    let created = PersistedConfigService::patch_config(&rpc, BaseController::default(), request)
        .await
        .unwrap();
    assert_eq!(
        created.status,
        PersistedConfigMutationStatus::Success as i32
    );
    // Lifecycle validation needs no TUN privileges or occupied listener ports.
    let saved = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        PatchPersistedConfigRequest {
            inst_id: Some(id.into()),
            expected_revision: created.entry.unwrap().revision,
            config: Some(NetworkConfig {
                no_tun: Some(true),
                disable_upnp: Some(true),
                bind_device: Some(false),
                ..Default::default()
            }),
            field_mask: vec!["no_tun".into(), "disable_upnp".into(), "bind_device".into()],
            toml: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(saved.status, PersistedConfigMutationStatus::Success as i32);
    WebClientService::set_network_instance_enabled(
        &rpc,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: true,
            expected_revision: Some(saved.entry.unwrap().revision),
        },
    )
    .await
    .unwrap();
    assert_eq!(manager.instance_ids(), vec![id]);
    let active = observe(&rpc)
        .await
        .entries
        .into_iter()
        .find(|entry| entry.inst_id == Some(id.into()))
        .unwrap();
    assert!(active.enabled);
    WebClientService::set_network_instance_enabled(
        &rpc,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: false,
            expected_revision: Some(active.revision),
        },
    )
    .await
    .unwrap();
    assert!(manager.instance_ids().is_empty());
    drop(rpc);
    drop(manager);
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        directory.path().to_owned(),
    )));
    let rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(DefaultHooks),
        storage,
        Arc::new(InstanceStateStore::new(Some(directory.path()))),
    );
    let stopped = observe(&rpc)
        .await
        .entries
        .into_iter()
        .find(|entry| entry.inst_id == Some(id.into()))
        .unwrap();
    assert!(!stopped.enabled && !stopped.running);
    assert_eq!(stopped.config.unwrap().no_tun, Some(true));
    assert!(manager.instance_ids().is_empty());
}

fn initial(id: uuid::Uuid, hostname: &str) -> String {
    format!(
        "# keep this comment\ninstance_id = '{id}'\nhostname = '{hostname}'\nnetns = 'keep-netns'\n[network_identity]\nnetwork_name = 'mesh'\nnetwork_secret = 'secret'\n[flags]\nno_tun = true\n"
    )
}

fn live_config(id: uuid::Uuid, hostname: &str) -> String {
    format!(
        "instance_id = '{id}'\ninstance_name = '{id}'\nhostname = '{hostname}'\nlisteners = []\n[network_identity]\nnetwork_name = 'live-mesh'\nnetwork_secret = 'secret'\n[flags]\nno_tun = true\nbind_device = false\ndisable_upnp = true\n"
    )
}

async fn wait_for_live_config(
    manager: &NativeInstanceManager,
    id: uuid::Uuid,
    hostname: Option<&str>,
) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let matches = match hostname {
                Some(hostname) => {
                    manager
                        .config(id)
                        .is_some_and(|config| config.get_hostname() == hostname)
                        && manager.instance(id).is_some_and(|instance| {
                            instance.state() == easytier_core::instance::CoreInstanceState::Running
                        })
                }
                None => manager.instance(id).is_none(),
            };
            if matches {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native directory notifications must apply edits without an ObserveConfigs request");
}

#[tokio::test]
async fn repeated_native_list_refreshes_keep_order_revisions_and_lifecycle_actions_usable() {
    use super::config_storage::NativeConfigFileStorage;

    let directory = tempfile::tempdir().unwrap();
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        directory.path().to_owned(),
    )));
    let state = Arc::new(InstanceStateStore::new(Some(directory.path())));
    let ids = (1..=8).map(uuid::Uuid::from_u128).collect::<Vec<_>>();
    for id in &ids {
        state.set_enabled(*id, false).unwrap();
        std::fs::write(
            directory.path().join(format!("{id}.toml")),
            live_config(*id, "paused"),
        )
        .unwrap();
    }
    std::fs::write(directory.path().join("broken.toml"), "[flags\n").unwrap();
    let rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(DefaultHooks),
        Arc::new(NativeConfigFileStorage::new(true)),
        state,
    );
    let baseline = observe(&rpc).await;
    for _ in 0..16 {
        let listed = WebClientService::list_network_instance(
            &rpc,
            BaseController::default(),
            ListNetworkInstanceRequest {},
        )
        .await
        .unwrap();
        assert_eq!(
            listed
                .disabled_inst_ids
                .into_iter()
                .map(uuid::Uuid::from)
                .collect::<Vec<_>>(),
            ids,
            "unchanged native files must return a stable, sorted network list"
        );
        assert_eq!(observe(&rpc).await, baseline);
    }
    assert!(baseline.entries.iter().any(|entry| {
        entry.entry_key == "broken.toml" && entry.inst_id.is_none() && entry.status == "invalid"
    }));
    let revision = |id: uuid::Uuid| {
        baseline
            .entries
            .iter()
            .find(|entry| entry.inst_id == Some(id.into()))
            .unwrap()
            .revision
            .clone()
    };
    WebClientService::set_network_instance_enabled(
        &rpc,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(ids[0].into()),
            enabled: true,
            expected_revision: Some(revision(ids[0])),
        },
    )
    .await
    .unwrap();
    wait_for_live_config(&manager, ids[0], Some("paused")).await;
    let active = manager.instance(ids[0]).unwrap();
    let running = observe(&rpc).await;
    for _ in 0..4 {
        WebClientService::list_network_instance(
            &rpc,
            BaseController::default(),
            ListNetworkInstanceRequest {},
        )
        .await
        .unwrap();
        assert_eq!(observe(&rpc).await, running);
        assert!(Arc::ptr_eq(&active, &manager.instance(ids[0]).unwrap()));
    }
    let edited = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(ids[1], revision(ids[1]), "edited"),
    )
    .await
    .unwrap();
    assert_eq!(edited.status, PersistedConfigMutationStatus::Success as i32);
    let deleted = WebClientService::delete_network_instance(
        &rpc,
        BaseController::default(),
        DeleteNetworkInstanceRequest {
            inst_ids: vec![ids[1].into()],
            expected_revision: Some(edited.entry.unwrap().revision),
        },
    )
    .await
    .unwrap();
    assert!(!deleted.remain_inst_ids.contains(&ids[1].into()));
    assert!(!directory.path().join(format!("{}.toml", ids[1])).exists());
    assert!(Arc::ptr_eq(&active, &manager.instance(ids[0]).unwrap()));
}

#[tokio::test]
async fn retiring_file_owner_drains_queued_scans_before_the_next_mode_starts() {
    use super::config_storage::NativeConfigFileStorage;

    let directory = tempfile::tempdir().unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    drop(socket);
    let id = uuid::Uuid::new_v4();
    let path = directory.path().join(format!("{id}.toml"));
    let raw = live_config(id, "mode-owner").replace(
        "listeners = []",
        &format!("listeners = ['tcp://{address}']"),
    );
    std::fs::write(&path, &raw).unwrap();
    let storage = Arc::new(NativeConfigFileStorage::new(true));

    // Alternate independent ordinary/service owners of the same network. The
    // old RPC/catalog can remain referenced by a UI while its runtime retires.
    for _ in 0..3 {
        let manager = Arc::new(native_instance_manager_with_config_dir(Some(
            directory.path().to_owned(),
        )));
        let state = Arc::new(InstanceStateStore::new(Some(directory.path())));
        let rpc = Arc::new(ProcessManagementRpc::new(
            manager.clone(),
            Arc::new(DefaultHooks),
            storage.clone(),
            state.clone(),
        ));
        wait_for_live_config(&manager, id, Some("mode-owner")).await;
        assert!(std::net::TcpListener::bind(address).is_err());
        let revision = observe(&rpc).await.entries[0].revision.clone();

        let lock = manager.mutation_lock();
        let operation = lock.lock().await;
        let queued_rpc = rpc.clone();
        let queued_save = tokio::spawn(async move {
            PersistedConfigService::patch_config(
                queued_rpc.as_ref(),
                BaseController::default(),
                patch(id, revision, "stale-edit"),
            )
            .await
        });
        manager.local_config_catalog().request_refresh();
        // Let the scan finish its debounce and queue behind this coordinator.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        manager.local_config_catalog().retire();
        manager.retain_network_instances(&[]).await.unwrap();
        assert!(
            state.is_enabled(&id),
            "switching modes must retain desired state"
        );
        drop(operation);
        assert!(
            queued_save.await.unwrap().is_err(),
            "queued old-mode saves must be rejected"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        assert!(
            manager
                .run_network_instance(
                    TomlConfigLoader::new_from_str(&raw).unwrap(),
                    ConfigFileControl::new(Some(path.clone()), ConfigFilePermission::default()),
                )
                .is_err(),
            "a stale caller must not restart the old mode"
        );
        // Queued scans and further filesystem events must not resurrect the
        // retiring owner, even though its manager and RPC remain alive.
        std::fs::write(&path, &raw).unwrap();
        manager.local_config_catalog().request_refresh();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            manager.instance_ids().is_empty(),
            "retired owner restarted its network"
        );
        let released = std::net::TcpListener::bind(address).expect("old listener was retained");
        drop(released);
        drop(rpc);
    }
}

#[tokio::test]
async fn live_native_files_add_edit_replace_rename_and_delete_without_polling() {
    use super::config_storage::NativeConfigFileStorage;
    let directory = tempfile::tempdir().unwrap();
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        directory.path().to_owned(),
    )));
    let state = Arc::new(InstanceStateStore::new(Some(directory.path())));
    let storage = Arc::new(NativeConfigFileStorage::new(true));
    let rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(DefaultHooks),
        storage.clone(),
        state.clone(),
    );
    let id = uuid::Uuid::new_v4();
    let path = directory.path().join(format!("{id}.toml"));
    storage
        .write(&path, live_config(id, "first").as_bytes())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("first")).await;
    assert_eq!(
        InstanceStateStore::new(Some(directory.path())).enabled_state(&id),
        Some(true),
        "manual additions must persist their default enabled state without a console read"
    );
    let original = manager.instance(id).unwrap();
    storage
        .write(
            &path,
            format!("# a comment\n{}", live_config(id, "first")).as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        Arc::ptr_eq(&original, &manager.instance(id).unwrap()),
        "comments must not restart a network"
    );
    storage
        .write(&path, b"[flags]\nno_tun = 'invalid'")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        Arc::ptr_eq(&original, &manager.instance(id).unwrap()),
        "a partially written or invalid file must preserve the running network"
    );
    storage
        .write(&path, live_config(id, "second").as_bytes())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("second")).await;
    assert!(!Arc::ptr_eq(&original, &manager.instance(id).unwrap()));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        live_config(id, "second"),
        "local application must never rewrite the user's document"
    );
    let renamed = directory.path().join("renamed.toml");
    std::fs::rename(&path, &renamed).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while manager
            .config_control(id)
            .is_none_or(|control| control.path.as_ref() != Some(&renamed))
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    wait_for_live_config(&manager, id, Some("second")).await;
    std::fs::remove_file(&renamed).unwrap();
    wait_for_live_config(&manager, id, None).await;
    // Shutdown can yield after removing the runtime but before persisting its
    // state. Wait for the whole reconciliation, without requesting a refresh.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.enabled_state(&id).is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        InstanceStateStore::new(Some(directory.path())).enabled_state(&id),
        None,
        "manual deletion must remove the enabled record from disk without a console read"
    );
    let snapshot = observe(&rpc).await;
    assert!(snapshot.entries.is_empty());
    assert!(manager.managed_config_ids().is_empty());
    assert_eq!(state.enabled_state(&id), None);
    // Recreating identical bytes must be a new local addition, even if an RPC
    // had previously acknowledged that same file content.
    storage
        .write(&path, live_config(id, "second").as_bytes())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("second")).await;
    assert_eq!(
        InstanceStateStore::new(Some(directory.path())).enabled_state(&id),
        Some(true)
    );
}

#[tokio::test]
async fn live_native_scan_reaps_offline_deletions_and_preserves_existing_pauses() {
    use super::config_storage::NativeConfigFileStorage;
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(InstanceStateStore::new(Some(directory.path())));
    let removed_enabled = uuid::Uuid::new_v4();
    let removed_disabled = uuid::Uuid::new_v4();
    let paused = uuid::Uuid::new_v4();
    let added = uuid::Uuid::new_v4();
    state.set_enabled(removed_enabled, true).unwrap();
    state.set_enabled(removed_disabled, false).unwrap();
    state.set_enabled(paused, false).unwrap();
    for (id, name) in [(paused, "paused"), (added, "manual")] {
        std::fs::write(
            directory.path().join(format!("{id}.toml")),
            live_config(id, name),
        )
        .unwrap();
    }
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        directory.path().to_owned(),
    )));
    let _rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(DefaultHooks),
        Arc::new(NativeConfigFileStorage::new(true)),
        state,
    );
    wait_for_live_config(&manager, added, Some("manual")).await;
    assert!(manager.instance(paused).is_none());
    let reloaded = InstanceStateStore::new(Some(directory.path()));
    assert_eq!(reloaded.enabled_state(&added), Some(true));
    assert_eq!(reloaded.enabled_state(&paused), Some(false));
    assert_eq!(reloaded.enabled_state(&removed_enabled), None);
    assert_eq!(reloaded.enabled_state(&removed_disabled), None);
}

#[tokio::test]
async fn live_native_files_apply_console_saves_preserve_explicit_pause_and_reap_deleted_state() {
    use super::config_storage::NativeConfigFileStorage;
    let directory = tempfile::tempdir().unwrap();
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        directory.path().to_owned(),
    )));
    let state = Arc::new(InstanceStateStore::new(Some(directory.path())));
    let storage = Arc::new(NativeConfigFileStorage::new(true));
    let rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(DefaultHooks),
        storage.clone(),
        state.clone(),
    );
    let id = uuid::Uuid::new_v4();
    let path = directory.path().join(format!("{id}.toml"));
    storage
        .write(&path, live_config(id, "active").as_bytes())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("active")).await;
    let revision = observe(&rpc).await.entries[0].revision.clone();
    let saved = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(id, revision, "saved"),
    )
    .await
    .unwrap();
    assert_eq!(saved.status, PersistedConfigMutationStatus::Success as i32);
    // A console save is applied exactly like a local edit of the file.
    assert_eq!(manager.config(id).unwrap().get_hostname(), "saved");
    let applied = manager.instance(id).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        Arc::ptr_eq(&applied, &manager.instance(id).unwrap()),
        "the watcher must not restart a network for its own console save"
    );
    let saved_contents = std::fs::read(&path).unwrap();
    storage
        .write(&path, live_config(id, "external").as_bytes())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("external")).await;
    // A later manual revert to previously acknowledged RPC bytes is still a
    // fresh external edit. Acknowledgements must be consumed only once.
    storage.write(&path, &saved_contents).await.unwrap();
    wait_for_live_config(&manager, id, Some("saved")).await;
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
    storage
        .write(&path, live_config(id, "paused-edit").as_bytes())
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(manager.instance(id).is_none());
    assert!(!state.is_enabled(&id));
    let paused_path = directory.path().join("paused-renamed.toml");
    std::fs::rename(&path, &paused_path).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        manager.instance(id).is_none(),
        "renaming a paused file must keep it paused"
    );
    assert!(!state.is_enabled(&id));
    assert_eq!(
        manager.managed_config_control(id).unwrap().path.as_ref(),
        Some(&paused_path)
    );
    std::fs::remove_file(&paused_path).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.enabled_state(&id).is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let listed = WebClientService::list_network_instance(
        &rpc,
        BaseController::default(),
        easytier_proto::api::manage::ListNetworkInstanceRequest {},
    )
    .await
    .unwrap();
    assert!(listed.inst_ids.is_empty() && listed.disabled_inst_ids.is_empty());
    assert!(observe(&rpc).await.entries.is_empty());
    storage
        .write(&path, live_config(id, "recreated").as_bytes())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("recreated")).await;
}

#[tokio::test]
async fn live_native_files_apply_env_protected_configs_and_isolate_duplicate_identities() {
    use super::config_storage::NativeConfigFileStorage;
    let directory = tempfile::tempdir().unwrap();
    let duplicate = uuid::Uuid::new_v4();
    let id = uuid::Uuid::new_v4();
    let path = directory.path().join(format!("{id}.toml"));
    let duplicate_path = directory.path().join(format!("{duplicate}.toml"));
    let alias = directory.path().join("alias.toml");
    std::fs::write(&duplicate_path, live_config(duplicate, "duplicate")).unwrap();
    std::fs::write(
        &alias,
        format!("instance_id='{duplicate}'\n[flags]\nno_tun='invalid'\n"),
    )
    .unwrap();
    let env = live_config(id, "env").replace(
        "network_secret = 'secret'",
        "network_secret = '${ET_LIVE_ENV_TEST:-fallback}'",
    );
    std::fs::write(&path, &env).unwrap();
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        directory.path().to_owned(),
    )));
    let rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(DefaultHooks),
        Arc::new(NativeConfigFileStorage::default()),
        Arc::new(InstanceStateStore::new(Some(directory.path()))),
    );
    wait_for_live_config(&manager, id, Some("env")).await;
    assert!(manager.instance(duplicate).is_none());
    assert_eq!(
        manager
            .config(id)
            .unwrap()
            .get_network_identity()
            .network_secret
            .as_deref(),
        Some("fallback")
    );
    let snapshot = observe(&rpc).await;
    let entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.inst_id == Some(id.into()))
        .unwrap();
    assert_eq!(entry.status, "protected");
    assert!(entry.persisted_toml.is_none() && entry.config.is_none());
    std::fs::write(
        &path,
        env.replace("hostname = 'env'", "hostname = 'env-edited'"),
    )
    .unwrap();
    wait_for_live_config(&manager, id, Some("env-edited")).await;
    std::fs::remove_file(&alias).unwrap();
    wait_for_live_config(&manager, duplicate, Some("duplicate")).await;
}

#[tokio::test]
async fn live_native_files_keep_the_previous_runtime_when_host_apply_fails() {
    use super::config_storage::NativeConfigFileStorage;
    use easytier_core::management::InstanceMutationHooks;
    struct RejectApply(std::sync::Weak<NativeInstanceManager>, Arc<Notify>);
    #[async_trait::async_trait]
    impl InstanceMutationHooks for RejectApply {
        async fn post_run_network_instance(&self, id: &uuid::Uuid) -> Result<(), String> {
            if self
                .0
                .upgrade()
                .and_then(|manager| manager.config(*id))
                .is_some_and(|config| config.get_hostname() == "rejected")
            {
                self.1.notify_one();
                return Err("simulated Host failure".into());
            }
            Ok(())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        directory.path().to_owned(),
    )));
    let failed = Arc::new(Notify::new());
    let storage = Arc::new(NativeConfigFileStorage::new(true));
    let _rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(RejectApply(Arc::downgrade(&manager), failed.clone())),
        storage.clone(),
        Arc::new(InstanceStateStore::new(Some(directory.path()))),
    );
    let id = uuid::Uuid::new_v4();
    let path = directory.path().join(format!("{id}.toml"));
    storage
        .write(&path, live_config(id, "working").as_bytes())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("working")).await;
    storage
        .write(&path, live_config(id, "rejected").as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), failed.notified())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("working")).await;
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        live_config(id, "rejected")
    );
    storage
        .write(&path, live_config(id, "fixed").as_bytes())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("fixed")).await;
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

fn patch(id: uuid::Uuid, revision: String, hostname: &str) -> PatchPersistedConfigRequest {
    PatchPersistedConfigRequest {
        inst_id: Some(id.into()),
        expected_revision: revision,
        config: Some(NetworkConfig {
            hostname: Some(hostname.into()),
            ..Default::default()
        }),
        field_mask: vec!["hostname".into()],
        toml: None,
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
            patch(id, revision.clone(), "one")
        ),
        PersistedConfigService::patch_config(
            &second,
            BaseController::default(),
            patch(id, revision, "two")
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
        patch(id, baseline.entries[0].revision.clone(), "stale"),
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
    let first_request = patch(id, revision.clone(), "first");
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
            patch(id, revision, "second"),
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
        patch(invalid, String::new(), "replace"),
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
async fn saving_a_disabled_config_does_not_enable_it() {
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
        patch(id, before.entries[0].revision.clone(), "saved"),
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
async fn saving_replaces_running_instance_and_keeps_original_non_form_fields() {
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
        patch(id, baseline.entries[0].revision.clone(), "applied"),
    )
    .await
    .unwrap();
    assert_eq!(result.status, PersistedConfigMutationStatus::Success as i32);
    assert!(!Arc::ptr_eq(&running, &manager.instance(id).unwrap()));
    assert_eq!(manager.config(id).unwrap().get_hostname(), "applied");
    let raw = String::from_utf8(files.read(&path(id)).await.unwrap().unwrap()).unwrap();
    assert!(raw.starts_with("# keep this comment"));
    assert!(raw.contains("unmodeled = 'keep'"));
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn typed_patches_require_a_mask_and_replacements_carry_no_typed_fields() {
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let raw = initial(id, "stopped").into_bytes();
    files.files.lock().unwrap().insert(path(id), raw.clone());
    let before = observe(&rpc).await;
    let revision = before.entries[0].revision.clone();
    let mut unmasked = patch(id, revision.clone(), "ignored-edit");
    unmasked.field_mask.clear();
    let mut mixed = patch(id, revision.clone(), "ignored-edit");
    mixed.toml = Some(initial(id, "replaced"));
    let mut missing_config = patch(id, revision, "ignored-edit");
    missing_config.config = None;
    for request in [unmasked, mixed, missing_config] {
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
    assert_eq!(files.read(&path(id)).await.unwrap().unwrap(), raw);
    assert_eq!(files.writes.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn toml_replacement_writes_the_exact_document_and_applies_it() {
    let (manager, files, rpc) = fixture();
    let id = uuid::Uuid::new_v4();
    let original = initial(id, "active").replace("netns = 'keep-netns'\n", "");
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), original.clone().into_bytes());
    start_saved_instance(&manager, &rpc, id).await;
    let revision = observe(&rpc).await.entries[0].revision.clone();
    let document = original.replace(
        "hostname = 'active'",
        "hostname = 'replaced'\n# trailing note",
    );
    let result = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        PatchPersistedConfigRequest {
            inst_id: Some(id.into()),
            expected_revision: revision.clone(),
            toml: Some(document.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, PersistedConfigMutationStatus::Success as i32);
    assert_eq!(
        files.read(&path(id)).await.unwrap().unwrap(),
        document.as_bytes()
    );
    assert_eq!(manager.config(id).unwrap().get_hostname(), "replaced");
    let entry = result.entry.unwrap();
    assert_eq!(entry.persisted_toml.as_deref(), Some(document.as_str()));
    assert_ne!(entry.revision, revision);

    for invalid in [
        format!("{document}\nunknown_field = true\n"),
        "hostname = [ broken".to_string(),
        document.replace(&format!("instance_id = '{id}'\n"), ""),
        initial(uuid::Uuid::new_v4(), "foreign"),
    ] {
        let rejected = PersistedConfigService::patch_config(
            &rpc,
            BaseController::default(),
            PatchPersistedConfigRequest {
                inst_id: Some(id.into()),
                expected_revision: entry.revision.clone(),
                toml: Some(invalid.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rejected.status,
            PersistedConfigMutationStatus::Invalid as i32,
            "{invalid}"
        );
        assert_eq!(
            files.read(&path(id)).await.unwrap().unwrap(),
            document.as_bytes()
        );
    }
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn failed_save_restores_the_previous_file_and_runtime() {
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
    let before = observe(&first).await.entries.remove(0);
    let applying = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(FailAfterRun),
        files.clone(),
        Arc::new(InstanceStateStore::in_memory()),
    );
    let result = PersistedConfigService::patch_config(
        &applying,
        BaseController::default(),
        patch(id, before.revision.clone(), "rejected"),
    )
    .await
    .unwrap();
    assert_eq!(
        result.status,
        PersistedConfigMutationStatus::ApplyFailed as i32
    );
    let entry = result.entry.unwrap();
    assert!(entry.running);
    assert_eq!(entry.revision, before.revision);
    assert_eq!(manager.config(id).unwrap().get_hostname(), "active");
    assert_eq!(
        files.read(&path(id)).await.unwrap().unwrap(),
        original.as_bytes()
    );
    manager.delete_network_instances([id]).await.unwrap();
}

#[tokio::test]
async fn legacy_hot_patch_preserves_unapplied_disk_fields_and_comments() {
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
async fn lifecycle_revisions_survive_runtime_state_changes_but_not_file_edits() {
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
    let paused = observe(&rpc).await.entries.remove(0);
    assert!(!paused.enabled);
    assert_eq!(
        paused.revision, revision,
        "an open editor must not conflict with a pause"
    );
    files
        .files
        .lock()
        .unwrap()
        .insert(path(id), initial(id, "edited").into_bytes());
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
async fn revision_capability_requires_observation_and_legacy_runtime_without_directory_still_works()
{
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
        assert!(
            !manager
                .management_capabilities()
                .iter()
                .any(|cap| cap.starts_with("management:persisted-config-revision"))
        );
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
        patch(id, revision, "written"),
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
            toml: None,
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
        patch(id, String::new(), "overwrite"),
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

#[tokio::test]
async fn live_native_files_preserve_implicit_secure_identity_and_stable_observations() {
    use super::config_storage::NativeConfigFileStorage;
    let directory = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4();
    let path = directory.path().join(format!("{id}.toml"));
    let secure = |hostname: &str| {
        format!(
            "{}[secure_mode]\nenabled = true\n",
            live_config(id, hostname)
        )
    };
    let manager = Arc::new(native_instance_manager_with_config_dir(Some(
        directory.path().to_owned(),
    )));
    let state = Arc::new(InstanceStateStore::new(Some(directory.path())));
    state.set_enabled(id, false).unwrap();
    let storage = Arc::new(NativeConfigFileStorage::new(true));
    let rpc = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(DefaultHooks),
        storage.clone(),
        state,
    );
    storage
        .write(&path, secure("secure-first").as_bytes())
        .await
        .unwrap();
    let stopped = observe(&rpc).await;
    assert!(!stopped.entries[0].running);
    assert!(
        stopped.entries[0]
            .config
            .as_ref()
            .unwrap()
            .secure_mode
            .as_ref()
            .unwrap()
            .local_private_key
            .is_none()
    );
    assert_eq!(
        stopped,
        observe(&rpc).await,
        "stopped implicit identity must not change on every observation"
    );
    WebClientService::set_network_instance_enabled(
        &rpc,
        BaseController::default(),
        SetNetworkInstanceEnabledRequest {
            inst_id: Some(id.into()),
            enabled: true,
            expected_revision: Some(stopped.entries[0].revision.clone()),
        },
    )
    .await
    .unwrap();
    wait_for_live_config(&manager, id, Some("secure-first")).await;
    let original = manager.instance(id).unwrap();
    let identity = manager.config(id).unwrap().get_secure_mode().unwrap();
    assert!(identity.local_private_key.is_some());
    let running = observe(&rpc).await;
    assert_eq!(
        running,
        observe(&rpc).await,
        "active implicit identity must remain stable across catalog scans"
    );
    storage
        .write(
            &path,
            format!("# format-only change\n{}", secure("secure-first")).as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        Arc::ptr_eq(&original, &manager.instance(id).unwrap()),
        "a secure file's comment edit must not regenerate its identity or restart it"
    );
    assert_eq!(
        manager.config(id).unwrap().get_secure_mode(),
        Some(identity.clone())
    );
    storage
        .write(&path, secure("secure-updated").as_bytes())
        .await
        .unwrap();
    wait_for_live_config(&manager, id, Some("secure-updated")).await;
    assert!(!Arc::ptr_eq(&original, &manager.instance(id).unwrap()));
    assert_eq!(
        manager.config(id).unwrap().get_secure_mode(),
        Some(identity.clone())
    );
    let updated = observe(&rpc).await;
    assert_eq!(updated, observe(&rpc).await);
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("local_private_key"),
        "local application must not rewrite implicit keys into the user's TOML"
    );
    let saved = PersistedConfigService::patch_config(
        &rpc,
        BaseController::default(),
        patch(
            id,
            updated.entries[0].revision.clone(),
            "secure-rpc-updated",
        ),
    )
    .await
    .unwrap();
    assert_eq!(saved.status, PersistedConfigMutationStatus::Success as i32);
    wait_for_live_config(&manager, id, Some("secure-rpc-updated")).await;
    assert_eq!(
        manager.config(id).unwrap().get_secure_mode(),
        Some(identity)
    );
    for blank in ["", " "] {
        let active = manager.instance(id).unwrap();
        let invalid = format!(
            "{}local_private_key = '{blank}'\n",
            secure("secure-updated")
        );
        storage
            .write(
                &directory.path().join(format!("{id}.toml")),
                invalid.as_bytes(),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            Arc::ptr_eq(&active, &manager.instance(id).unwrap()),
            "an invalid blank explicit key must preserve the prior runtime"
        );
    }
}
