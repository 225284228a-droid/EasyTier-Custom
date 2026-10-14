use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use easytier::{
    common::config::{ConfigLoader as _, NetworkConfigExt as _},
    proto::{
        api::manage::{
            NetworkConfig, NetworkingMethod, ObserveConfigsRequest, PersistedConfigService as _,
            PersistedConfigServiceServer, WebClientServiceServer,
        },
        rpc::bidirect::BidirectRpcManager,
        rpc_types::controller::BaseController,
        web::{HeartbeatRequest, WebServerServiceClientFactory},
    },
};
use easytier_core::{
    management::{
        ConfigFileControl, ConfigFilePermission, ConfigFileStorage, InstanceStateStore,
        ProcessManagementRpc,
    },
    tunnel::ring::create_ring_tunnel_pair,
};
use uuid::Uuid;

use super::{ClientManager, HeartbeatPolicy, session::Session};
use crate::{FeatureFlags, central_network::local_delivery, db::Db, webhook::ManagedNetworkConfig};

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
            .filter(|path| path.parent() == Some(directory))
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

fn desired(instance_id: Uuid, hostname: &str) -> NetworkConfig {
    NetworkConfig {
        instance_id: Some(instance_id.to_string()),
        network_name: Some(format!("central-local-{instance_id}")),
        network_secret: Some("central-secret".into()),
        hostname: Some(hostname.into()),
        networking_method: Some(NetworkingMethod::Standalone as i32),
        secure_mode: Some(easytier::proto::common::SecureModeConfig {
            enabled: true,
            ..Default::default()
        }),
        no_tun: Some(true),
        disable_p2p: Some(true),
        ..Default::default()
    }
}

async fn publish(manager: &ClientManager, user: i32, machine: Uuid, config: NetworkConfig) {
    publish_many(manager, user, machine, vec![config]).await;
}

async fn publish_many(
    manager: &ClientManager,
    user: i32,
    machine: Uuid,
    configs: Vec<NetworkConfig>,
) {
    local_delivery::publish(
        manager.storage.db(),
        manager,
        user,
        machine,
        configs
            .into_iter()
            .map(|config| ManagedNetworkConfig {
                instance_id: config.instance_id.clone().unwrap(),
                network_config: serde_json::to_value(config).unwrap(),
            })
            .collect(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn central_local_delivery_creates_updates_deletes_and_preserves_local_files() {
    let db = Db::memory_db().await;
    let user = db.auto_create_user("central-local-test").await.unwrap().id;
    let machine = Uuid::new_v4();
    let instance = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    let files = Arc::new(Files::default());
    let directory = PathBuf::from(format!("central-local-fixture-{machine}"));
    let owned_path = directory.join(format!("{instance}.toml"));
    let foreign_path = directory.join(format!("{foreign}.toml"));
    let foreign_contents = desired(foreign, "local-user")
        .gen_config()
        .unwrap()
        .dump()
        .into_bytes();
    files
        .0
        .lock()
        .unwrap()
        .insert(foreign_path.clone(), foreign_contents.clone());
    let core = Arc::new(
        easytier::instance::factory::native_instance_manager_with_config_dir(Some(directory)),
    );
    let process = ProcessManagementRpc::new(
        core.clone(),
        Arc::new(easytier::web_client::DefaultHooks),
        files.clone(),
        Arc::new(InstanceStateStore::in_memory()),
    );
    let webhook = Arc::new(crate::webhook::WebhookConfig::new(
        None, None, None, None, None,
    ));
    let manager = ClientManager::new(
        db.clone(),
        None,
        HeartbeatPolicy::default(),
        Arc::new(FeatureFlags::default()),
        webhook.clone(),
    );
    let mut session = Session::new(
        manager.storage.weak_ref(),
        "tcp://127.0.0.1:2391".parse().unwrap(),
        None,
        HeartbeatPolicy::default(),
        Arc::new(FeatureFlags::default()),
        webhook,
        1,
    );
    let node_rpc = BidirectRpcManager::new();
    node_rpc
        .rpc_server()
        .registry()
        .register(PersistedConfigServiceServer::new(process.clone()), "");
    node_rpc
        .rpc_server()
        .registry()
        .register(WebClientServiceServer::new(process.clone()), "");
    let (web, node) = create_ring_tunnel_pair();
    session.serve(web).await;
    session.mark_route_ready();
    let session = Arc::new(session);
    manager
        .client_sessions
        .insert("tcp://127.0.0.1:2391".parse().unwrap(), session.clone());
    node_rpc.run_with_tunnel(node);
    let snapshot = process
        .observe_configs(BaseController::default(), ObserveConfigsRequest {})
        .await
        .unwrap();
    node_rpc
        .rpc_client()
        .scoped_client::<WebServerServiceClientFactory<BaseController>>(1, 1, String::new())
        .heartbeat(
            BaseController::default(),
            HeartbeatRequest {
                machine_id: Some(machine.into()),
                user_token: "central-local-test".into(),
                report_time: chrono::Utc::now().to_rfc3339(),
                support_local_configs: true,
                support_local_config_revision: true,
                local_config_catalog_epoch: snapshot.catalog_epoch,
                local_config_catalog_generation: snapshot.catalog_generation,
                runtime_capabilities: vec![
                    "management:persisted-config-revision-v1".into(),
                    "management:persisted-config-apply-v1".into(),
                ],
                ..Default::default()
            },
        )
        .await
        .unwrap();

    publish(&manager, user, machine, desired(instance, "first")).await;
    assert!(core.instance(instance).is_some());
    assert!(
        db.central_local_delivery_error(user, machine, instance)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        String::from_utf8(files.0.lock().unwrap()[&owned_path].clone())
            .unwrap()
            .contains("first")
    );
    let identity = core.config(instance).unwrap().get_secure_mode();
    publish(&manager, user, machine, desired(instance, "updated")).await;
    assert_eq!(core.config(instance).unwrap().get_hostname(), "updated");
    assert_eq!(core.config(instance).unwrap().get_secure_mode(), identity);
    assert_eq!(files.0.lock().unwrap()[&foreign_path], foreign_contents);

    let second = Uuid::new_v4();
    let second_path = owned_path.with_file_name(format!("{second}.toml"));
    publish_many(
        &manager,
        user,
        machine,
        vec![
            desired(instance, "updated"),
            desired(second, "second-network"),
        ],
    )
    .await;
    assert!(core.instance(instance).is_some());
    assert!(core.instance(second).is_some());
    assert!(
        db.central_local_delivery_error(user, machine, second)
            .await
            .unwrap()
            .is_none()
    );

    // Removing central's identity never uses a catalog-wide retain/delete.
    local_delivery::publish(&db, &manager, user, machine, Vec::new())
        .await
        .unwrap();
    assert!(core.instance(instance).is_none());
    assert!(core.instance(second).is_none());
    assert!(!files.0.lock().unwrap().contains_key(&owned_path));
    assert!(!files.0.lock().unwrap().contains_key(&second_path));
    assert_eq!(files.0.lock().unwrap()[&foreign_path], foreign_contents);

    publish(&manager, user, machine, desired(instance, "published")).await;
    let manual = String::from_utf8(files.0.lock().unwrap()[&owned_path].clone())
        .unwrap()
        .replace("published", "manual-local");
    files
        .0
        .lock()
        .unwrap()
        .insert(owned_path.clone(), manual.clone().into_bytes());
    // Repeated publication is acknowledged already and preserves local edits.
    publish(&manager, user, machine, desired(instance, "published")).await;
    assert_eq!(files.0.lock().unwrap()[&owned_path], manual.as_bytes());
    publish(
        &manager,
        user,
        machine,
        desired(instance, "new-central-change"),
    )
    .await;
    assert!(
        db.central_local_delivery_error(user, machine, instance)
            .await
            .unwrap()
            .unwrap()
            .contains("conflicted")
    );
    assert_eq!(files.0.lock().unwrap()[&owned_path], manual.as_bytes());
    files.0.lock().unwrap().remove(&owned_path);
    // Returning to an acknowledged central desired state also cannot recreate
    // a file the node's owner has deliberately removed.
    publish(&manager, user, machine, desired(instance, "published")).await;
    assert!(!files.0.lock().unwrap().contains_key(&owned_path));
    assert_eq!(files.0.lock().unwrap()[&foreign_path], foreign_contents);

    session.stop().await;
    node_rpc.stop().await;
    core.retain_network_instances(&[]).await.unwrap();
}
