use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use easytier::proto::{
    api::manage::{
        NetworkConfig, ObserveConfigsResponse, PersistedConfigApplyMode,
        PersistedConfigServiceServer, WebClientServiceServer,
    },
    web::WebServerServiceClientFactory,
};
use easytier_core::{
    management::{
        ConfigFileControl, ConfigFilePermission, ConfigFileStorage, InstanceStateStore,
        ProcessManagementRpc,
    },
    tunnel::ring::create_ring_tunnel_pair,
};

use super::super::{BidirectRpcManager, SessionAuthState, StorageToken};
use super::*;
use crate::{
    client_manager::{FeatureFlags, HeartbeatPolicy, Storage},
    db::Db,
    webhook::WebhookConfig,
};

#[derive(Default)]
struct Files(Mutex<BTreeMap<PathBuf, Vec<u8>>>);

#[async_trait::async_trait]
impl ConfigFileStorage for Files {
    fn supports_catalog(&self) -> bool {
        true
    }
    async fn list_configs(&self, directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .keys()
            .filter(|p| p.parent() == Some(directory))
            .cloned()
            .collect())
    }
    async fn inspect(&self, path: &Path) -> ConfigFileControl {
        ConfigFileControl::new(Some(path.to_owned()), ConfigFilePermission::from(0u32))
    }
    async fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.0.lock().unwrap().get(path).cloned())
    }
    async fn write(&self, path: &Path, contents: &[u8]) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(path.to_owned(), contents.to_vec());
        Ok(())
    }
    async fn remove(&self, path: &Path) -> anyhow::Result<()> {
        self.0.lock().unwrap().remove(path);
        Ok(())
    }
}

fn heartbeat_request(machine: uuid::Uuid, snapshot: &ObserveConfigsResponse) -> HeartbeatRequest {
    HeartbeatRequest {
        machine_id: Some(machine.into()),
        user_token: "mirror-owner".into(),
        report_time: chrono::Utc::now().to_rfc3339(),
        support_heartbeat_policy: true,
        support_local_configs: true,
        support_local_config_revision: true,
        local_config_catalog_epoch: snapshot.catalog_epoch.clone(),
        local_config_catalog_generation: snapshot.catalog_generation,
        runtime_capabilities: vec![REVISION_CAPABILITY.into()],
        ..Default::default()
    }
}

async fn connect_console(
    storage: &Storage,
    process: &ProcessManagementRpc<easytier::instance::factory::NativeInstanceFactory>,
    port: u16,
) -> (Arc<Session>, BidirectRpcManager) {
    let mut session = Session::new(
        storage.weak_ref(),
        format!("tcp://127.0.0.1:{port}").parse().unwrap(),
        None,
        HeartbeatPolicy::default(),
        Arc::new(FeatureFlags {
            allow_auto_create_user: true,
            ..Default::default()
        }),
        Arc::new(WebhookConfig::new(None, None, None, None, None)),
        1,
    );
    let rpc = BidirectRpcManager::new();
    rpc.rpc_server()
        .registry()
        .register(PersistedConfigServiceServer::new(process.clone()), "");
    rpc.rpc_server()
        .registry()
        .register(WebClientServiceServer::new(process.clone()), "");
    let (web, node) = create_ring_tunnel_pair();
    session.serve(web).await;
    session.mark_route_ready();
    rpc.run_with_tunnel(node);
    (Arc::new(session), rpc)
}

async fn send_heartbeat(
    rpc: &BidirectRpcManager,
    machine: uuid::Uuid,
    snapshot: &ObserveConfigsResponse,
) {
    rpc.rpc_client()
        .scoped_client::<WebServerServiceClientFactory<BaseController>>(1, 1, String::new())
        .heartbeat(
            BaseController::default(),
            heartbeat_request(machine, snapshot),
        )
        .await
        .unwrap();
}

async fn await_mirrors(
    stores: &[Storage],
    users: &[i32],
    machine: uuid::Uuid,
    generation: u64,
    hostname: &str,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut complete = true;
            for (storage, user) in stores.iter().zip(users) {
                let mirror = storage
                    .db()
                    .local_config_mirror(*user, machine)
                    .await
                    .unwrap();
                complete &= mirror.as_ref().is_some_and(|m| {
                    m.snapshot.catalog_generation >= generation
                        && m.snapshot
                            .entries
                            .first()
                            .and_then(|e| e.config.as_ref())
                            .and_then(|c| c.hostname.as_deref())
                            == Some(hostname)
                });
            }
            if complete {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("both independent databases must refresh within ten seconds");
}

#[tokio::test]
async fn independent_servers_share_node_cas_and_observe_manual_edits() {
    let id = uuid::Uuid::new_v4();
    let machine = uuid::Uuid::new_v4();
    let directory = PathBuf::from("mirror-fixture");
    let path = directory.join(format!("{id}.toml"));
    let initial = format!(
        "# preserve comment\ninstance_id = '{id}'\nhostname = 'base'\nnetns = 'keep-me'\n[network_identity]\nnetwork_name = 'mesh'\nnetwork_secret = 'secret'\n[flags]\nno_tun = true\n"
    );
    let files = Arc::new(Files::default());
    files
        .0
        .lock()
        .unwrap()
        .insert(path.clone(), initial.as_bytes().to_vec());
    let manager = Arc::new(
        easytier::instance::factory::native_instance_manager_with_config_dir(Some(directory)),
    );
    let state = Arc::new(InstanceStateStore::in_memory());
    state.set_enabled(id, false).unwrap();
    let process = ProcessManagementRpc::new(
        manager.clone(),
        Arc::new(easytier::web_client::DefaultHooks),
        files.clone(),
        state,
    );
    let baseline = process
        .observe_configs(BaseController::default(), ObserveConfigsRequest {})
        .await
        .unwrap();
    let stores = [
        Storage::new(Db::memory_db().await),
        Storage::new(Db::memory_db().await),
    ];
    let mut users = Vec::new();
    for storage in &stores {
        users.push(
            storage
                .db()
                .auto_create_user("mirror-owner")
                .await
                .unwrap()
                .id,
        );
    }
    let (first, rpc1) = connect_console(&stores[0], &process, 2101).await;
    let (second, rpc2) = connect_console(&stores[1], &process, 2102).await;
    send_heartbeat(&rpc1, machine, &baseline).await;
    send_heartbeat(&rpc2, machine, &baseline).await;
    await_mirrors(
        &stores,
        &users,
        machine,
        baseline.catalog_generation,
        "base",
    )
    .await;
    let patch = |name: &str| PatchPersistedConfigRequest {
        inst_id: Some(id.into()),
        expected_revision: baseline.entries[0].revision.clone(),
        config: Some(NetworkConfig {
            hostname: Some(name.into()),
            ..Default::default()
        }),
        field_mask: vec!["hostname".into()],
        apply_mode: PersistedConfigApplyMode::SaveAndApply as i32,
    };
    let (a, b) = tokio::join!(
        first.patch_local_config(patch("first")),
        second.patch_local_config(patch("second"))
    );
    let winner = match (a, b) {
        (Ok(_), Err(LocalConfigError::Conflict(_))) => "first",
        (Err(LocalConfigError::Conflict(_)), Ok(_)) => "second",
        other => panic!("exactly one console must win its CAS: {other:?}"),
    };
    let saved = manager.local_config_catalog().snapshot();
    send_heartbeat(&rpc1, machine, &saved).await;
    send_heartbeat(&rpc2, machine, &saved).await;
    await_mirrors(&stores, &users, machine, saved.catalog_generation, winner).await;
    assert!(!saved.entries[0].enabled && !saved.entries[0].running);
    assert!(manager.instance_ids().is_empty());
    let raw = String::from_utf8(files.read(&path).await.unwrap().unwrap()).unwrap();
    assert!(raw.contains("# preserve comment") && raw.contains("netns = 'keep-me'"));
    // External edits bypass the mutation lock. The periodic directory scan,
    // rather than an explicit Observe RPC, must discover this change.
    let edit_started = tokio::time::Instant::now();
    files.0.lock().unwrap().insert(
        path.clone(),
        initial.replace("'base'", "'manual'").into_bytes(),
    );
    let manual = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = manager.local_config_catalog().snapshot();
            if snapshot.catalog_generation > saved.catalog_generation {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    send_heartbeat(&rpc1, machine, &manual).await;
    send_heartbeat(&rpc2, machine, &manual).await;
    await_mirrors(
        &stores,
        &users,
        machine,
        manual.catalog_generation,
        "manual",
    )
    .await;
    assert!(edit_started.elapsed() < Duration::from_secs(10));
    assert!(manager.instance_ids().is_empty());
    let started = first
        .mutate_local_config_lifecycle(id, manual.entries[0].revision.clone(), Some(true))
        .await
        .unwrap();
    assert!(started.snapshot.entries[0].running);
    let deleted = first
        .mutate_local_config_lifecycle(id, started.snapshot.entries[0].revision.clone(), None)
        .await
        .unwrap();
    assert!(deleted.snapshot.entries.is_empty());
    assert!(
        manager.instance_ids().is_empty(),
        "deleting a network must stop its running instance"
    );
    assert!(files.read(&path).await.unwrap().is_none());
    send_heartbeat(&rpc1, machine, &deleted.snapshot).await;
    send_heartbeat(&rpc2, machine, &deleted.snapshot).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut complete = true;
            for (storage, user) in stores.iter().zip(&users) {
                complete &= storage
                    .db()
                    .local_config_mirror(*user, machine)
                    .await
                    .unwrap()
                    .is_some_and(|m| m.snapshot.entries.is_empty());
            }
            if complete {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("deletion must reach both independent mirrors");
    first.stop().await;
    second.stop().await;
    rpc1.stop().await;
    rpc2.stop().await;
    assert!(
        stores[0]
            .db()
            .known_local_config_mode(users[0], machine)
            .await
            .unwrap()
    );
    assert!(
        stores[0]
            .db()
            .local_config_mirror(users[0], machine)
            .await
            .unwrap()
            .unwrap()
            .stale
    );
}

struct DelayedObservation {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    snapshot: ObserveConfigsResponse,
}
#[async_trait::async_trait]
impl PersistedConfigService for DelayedObservation {
    type Controller = BaseController;
    async fn observe_configs(
        &self,
        _: BaseController,
        _: ObserveConfigsRequest,
    ) -> Result<ObserveConfigsResponse, easytier::proto::rpc_types::error::Error> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(self.snapshot.clone())
    }
    async fn patch_config(
        &self,
        _: BaseController,
        _: PatchPersistedConfigRequest,
    ) -> Result<
        easytier::proto::api::manage::PatchPersistedConfigResponse,
        easytier::proto::rpc_types::error::Error,
    > {
        panic!("observation must never send a mutation")
    }
}

#[tokio::test]
async fn late_observation_cannot_replace_a_reconnected_session_snapshot() {
    let storage = Storage::new(Db::memory_db().await);
    let user = storage
        .db()
        .auto_create_user("mirror-owner")
        .await
        .unwrap()
        .id;
    let machine = uuid::Uuid::new_v4();
    let old = ObserveConfigsResponse {
        catalog_epoch: "old-node".into(),
        catalog_generation: 9,
        ..Default::default()
    };
    let session = Session::new(
        storage.weak_ref(),
        "tcp://127.0.0.1:2201".parse().unwrap(),
        None,
        HeartbeatPolicy::default(),
        Arc::new(FeatureFlags::default()),
        Arc::new(WebhookConfig::new(None, None, None, None, None)),
        1,
    );
    let token = StorageToken {
        token: "mirror-owner".into(),
        client_url: "tcp://127.0.0.1:2201".parse().unwrap(),
        machine_id: machine,
        user_id: user,
    };
    {
        let mut data = session.data.write().await;
        data.storage_token = Some(token.clone());
        data.auth_state = SessionAuthState::Authorized;
        data.req = Some(heartbeat_request(machine, &old));
    }
    storage.update_session_client(token.clone(), 1, true, 1);
    let delayed = Arc::new(DelayedObservation {
        started: Default::default(),
        release: Default::default(),
        snapshot: old,
    });
    let task = tokio::spawn({
        let data = session.data.clone();
        let delayed = delayed.clone();
        async move { observe(&data, delayed.as_ref()).await }
    });
    delayed.started.notified().await;
    storage.update_session_client(token, 2, true, 2);
    let fresh = ObserveConfigsResponse {
        catalog_epoch: "new-node".into(),
        catalog_generation: 1,
        ..Default::default()
    };
    storage
        .db()
        .store_local_config_snapshot(
            user,
            machine,
            storage.weak_ref().upgrade().unwrap().local_observer_epoch,
            2,
            &fresh,
            &[],
        )
        .await
        .unwrap();
    delayed.release.notify_one();
    assert!(task.await.unwrap().is_err());
    assert_eq!(
        storage
            .db()
            .local_config_mirror(user, machine)
            .await
            .unwrap()
            .unwrap()
            .snapshot,
        fresh
    );
    session.stop().await;
}

#[test]
fn masked_extensions_require_capability_even_when_false_or_empty() {
    let request = PatchPersistedConfigRequest {
        config: Some(NetworkConfig {
            enable_bbr: Some(false),
            sni: Some(String::new()),
            ..Default::default()
        }),
        field_mask: vec!["enable_bbr".into(), "sni".into()],
        ..Default::default()
    };
    assert!(matches!(
        validate_capability_mask(&request, &[]),
        Err(LocalConfigError::Unsupported)
    ));
    let mut unselected = request.clone();
    unselected.field_mask = vec!["hostname".into()];
    assert!(validate_capability_mask(&unselected, &[]).is_ok());
    assert!(
        validate_capability_mask(&request, &["config:enable_bbr".into(), "config:sni".into()])
            .is_ok()
    );
}

#[derive(Clone)]
struct UnfinishedWrite {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    snapshot: ObserveConfigsResponse,
}

#[async_trait::async_trait]
impl PersistedConfigService for UnfinishedWrite {
    type Controller = BaseController;
    async fn observe_configs(
        &self,
        _: BaseController,
        _: ObserveConfigsRequest,
    ) -> easytier::proto::rpc_types::error::Result<ObserveConfigsResponse> {
        Ok(self.snapshot.clone())
    }
    async fn patch_config(
        &self,
        _: BaseController,
        _: PatchPersistedConfigRequest,
    ) -> easytier::proto::rpc_types::error::Result<
        easytier::proto::api::manage::PatchPersistedConfigResponse,
    > {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::pending().await
    }
}

#[tokio::test]
async fn timed_out_write_observes_again_without_resending_the_mutation() {
    let storage = Storage::new(Db::memory_db().await);
    let user = storage
        .db()
        .auto_create_user("mirror-owner")
        .await
        .unwrap()
        .id;
    let machine = uuid::Uuid::new_v4();
    let snapshot = ObserveConfigsResponse {
        catalog_epoch: "timeout-node".into(),
        catalog_generation: 12,
        ..Default::default()
    };
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rpc = BidirectRpcManager::new();
    rpc.rpc_server().registry().register(
        PersistedConfigServiceServer::new(UnfinishedWrite {
            calls: calls.clone(),
            snapshot: snapshot.clone(),
        }),
        "",
    );
    let mut session = Session::new(
        storage.weak_ref(),
        "tcp://127.0.0.1:2301".parse().unwrap(),
        None,
        HeartbeatPolicy::default(),
        Arc::new(FeatureFlags::default()),
        Arc::new(WebhookConfig::new(None, None, None, None, None)),
        1,
    );
    let token = StorageToken {
        token: "mirror-owner".into(),
        client_url: "tcp://127.0.0.1:2301".parse().unwrap(),
        machine_id: machine,
        user_id: user,
    };
    {
        let mut data = session.data.write().await;
        data.storage_token = Some(token.clone());
        data.auth_state = SessionAuthState::Authorized;
        data.req = Some(heartbeat_request(machine, &snapshot));
    }
    storage.update_session_client(token, 1, true, 1);
    let (web, node) = create_ring_tunnel_pair();
    session.serve(web).await;
    rpc.run_with_tunnel(node);
    let result = tokio::time::timeout(
        Duration::from_secs(12),
        session.patch_local_config(PatchPersistedConfigRequest {
            inst_id: Some(uuid::Uuid::new_v4().into()),
            expected_revision: "base-revision".into(),
            config: Some(NetworkConfig {
                hostname: Some("new-name".into()),
                ..Default::default()
            }),
            field_mask: vec!["hostname".into()],
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(LocalConfigError::Unknown)));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        storage
            .db()
            .local_config_mirror(user, machine)
            .await
            .unwrap()
            .unwrap()
            .snapshot,
        snapshot
    );
    session.stop().await;
    rpc.stop().await;
}
