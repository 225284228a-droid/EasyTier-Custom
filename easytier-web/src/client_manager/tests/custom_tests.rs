use super::*;

#[test]
fn embedded_geoip_location_covers_global_ipv4_and_ipv6() {
    let db = Arc::new(super::super::load_geoip_db(None));
    let mut countries = std::collections::HashSet::new();
    for ip in ["223.5.5.5", "8.8.8.8", "2001:4860:4860::8888"] {
        let ip = ip.parse().unwrap();
        let country = db
            .as_ref()
            .as_ref()
            .unwrap()
            .lookup(ip)
            .unwrap()
            .decode::<maxminddb::geoip2::Country>()
            .unwrap()
            .unwrap();
        let code = country.country.iso_code.unwrap();
        assert_eq!(code.len(), 2);
        assert!(code.bytes().all(|value| value.is_ascii_uppercase()));
        countries.insert(code.to_string());
        let location = ClientManager::lookup_ip(ip, db.clone()).unwrap();
        assert_ne!(location.country, "海外");
        assert!(location.latitude.is_none());
        assert!(location.longitude.is_none());
    }
    // Anycast locations vary between database releases. Require genuine
    // global coverage instead of pinning their geography to ownership.
    assert!(countries.contains("CN"));
    assert!(countries.len() >= 2);
}

#[test]
fn full_snapshot_distinguishes_transient_empty_metadata_from_missing_ips() {
    use easytier::proto::api::manage::{
        CollectNetworkInfoResponse, MyNodeInfo, NetworkInstanceRunningInfo,
        NetworkInstanceRunningInfoMap,
    };
    let mut response = CollectNetworkInfoResponse::default();
    assert_eq!(ClientManager::snapshot_public_ips(&response), None);
    response.info = Some(NetworkInstanceRunningInfoMap::default());
    assert_eq!(ClientManager::snapshot_public_ips(&response), None);
    response.info.as_mut().unwrap().map.insert(
        "instance".to_string(),
        NetworkInstanceRunningInfo {
            running: true,
            my_node_info: Some(MyNodeInfo::default()),
            ..Default::default()
        },
    );
    assert_eq!(ClientManager::snapshot_public_ips(&response), Some(vec![]));
    response
        .info
        .as_mut()
        .unwrap()
        .map
        .get_mut("instance")
        .unwrap()
        .running = false;
    assert_eq!(ClientManager::snapshot_public_ips(&response), Some(vec![]));
}

#[tokio::test]
async fn stale_or_failed_city_reads_fall_back_to_offline_coordinates() {
    let offline = Arc::new(super::super::load_geoip_db(None));
    let cache = crate::geolocation::CityGeoCache::test_memory(crate::geolocation::Options {
        offline: true,
        ..Default::default()
    })
    .await;
    let ip = "1.1.1.1".parse().unwrap();
    let city = crate::geolocation::CityLocation {
        country: "New Zealand".to_string(),
        city: "Auckland".to_string(),
        region: None,
        latitude: -36.85,
        longitude: 174.76,
        accuracy_radius_km: None,
    };
    let at = chrono::Utc::now().timestamp();
    cache.test_seed(ip, &city, at).await;
    let fresh = ClientManager::cached_location(ip, &cache, offline.clone())
        .await
        .unwrap();
    assert_eq!(fresh.city.as_deref(), Some("Auckland"));
    assert_eq!(fresh.latitude, Some(-36.85));
    cache.test_seed(ip, &city, at - 8 * 86_400).await;
    let expected = ClientManager::lookup_ip(ip, offline.clone()).unwrap();
    let stale = ClientManager::cached_location(ip, &cache, offline.clone())
        .await
        .unwrap();
    assert_eq!(stale.country, expected.country);
    assert_eq!(stale.city, expected.city);
    assert_eq!(stale.latitude, expected.latitude);
    assert_eq!(stale.longitude, expected.longitude);
    cache.test_close().await;
    let failed = ClientManager::cached_location(ip, &cache, offline)
        .await
        .unwrap();
    assert_eq!(failed.country, expected.country);
    assert_eq!(failed.latitude, expected.latitude);
    assert_eq!(failed.longitude, expected.longitude);
}

fn dual_stack_geo_node() -> easytier::proto::api::manage::MyNodeInfo {
    easytier::proto::api::manage::MyNodeInfo {
        stun_info: Some(easytier::proto::common::StunInfo {
            public_ip: vec!["2001:4860:4860::8888".into(), "1.1.1.1".into()],
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn dual_stack_geo_manager() -> (ClientManager, Arc<crate::geolocation::CityGeoCache>) {
    let cache = crate::geolocation::CityGeoCache::test_memory(crate::geolocation::Options {
        offline: true,
        ..Default::default()
    })
    .await;
    let mut manager = ClientManager::new(
        Db::memory_db().await,
        None,
        HeartbeatPolicy::default(),
        Arc::new(FeatureFlags::default()),
        Arc::new(crate::webhook::WebhookConfig::new(
            None, None, None, None, None,
        )),
    );
    manager.city_geo_cache = Some(cache.clone());
    (manager, cache)
}

fn geo_city(name: &str, latitude: f64) -> crate::geolocation::CityLocation {
    crate::geolocation::CityLocation {
        country: "New Zealand".to_string(),
        city: name.to_string(),
        region: None,
        latitude,
        longitude: 174.76,
        accuracy_radius_km: None,
    }
}

#[tokio::test]
async fn ipv6_city_fallback_keeps_ipv4_display_and_ipv4_city_wins_when_available() {
    let (manager, cache) = dual_stack_geo_manager().await;
    let node = dual_stack_geo_node();
    let v4 = "1.1.1.1".parse().unwrap();
    let v6 = "2001:4860:4860::8888".parse().unwrap();
    cache
        .test_seed(
            v6,
            &geo_city("IPv6 City", -36.85),
            chrono::Utc::now().timestamp(),
        )
        .await;
    let (display, fallback) = manager.resolve_node_location(&node).await.unwrap();
    assert_eq!(display, v4);
    assert_eq!(fallback.city.as_deref(), Some("IPv6 City"));
    assert_eq!(fallback.latitude, Some(-36.85));
    cache
        .test_seed(
            v4,
            &geo_city("IPv4 City", -41.29),
            chrono::Utc::now().timestamp(),
        )
        .await;
    let (display, preferred) = manager.resolve_node_location(&node).await.unwrap();
    assert_eq!(display, v4);
    assert_eq!(preferred.city.as_deref(), Some("IPv4 City"));
    assert_eq!(preferred.latitude, Some(-41.29));
    let cached = ClientManager::resolve_node_ips(
        &crate::geolocation::node_public_ips(&node),
        Some(&cache),
        manager.geoip_db.clone(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(cached.city.as_deref(), Some("IPv4 City"));
}

#[tokio::test]
async fn every_verified_node_family_is_queued_without_cross_node_fallback() {
    let (manager, cache) = dual_stack_geo_manager().await;
    let node = dual_stack_geo_node();
    manager.resolve_node_location(&node).await.unwrap();
    let mut queued = cache.test_due_ips().await;
    queued.sort();
    let mut expected = crate::geolocation::node_public_ips(&node);
    expected.sort();
    assert_eq!(queued, expected);
    cache
        .test_seed(
            "2001:4860:4860::8888".parse().unwrap(),
            &geo_city("Other Node City", -36.85),
            chrono::Utc::now().timestamp(),
        )
        .await;
    let unrelated = easytier::proto::api::manage::MyNodeInfo {
        stun_info: Some(easytier::proto::common::StunInfo {
            public_ip: vec!["9.9.9.9".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let (display, location) = manager.resolve_node_location(&unrelated).await.unwrap();
    assert_eq!(display, "9.9.9.9".parse::<std::net::IpAddr>().unwrap());
    assert_ne!(location.city.as_deref(), Some("Other Node City"));
    assert!(location.latitude.is_none());
    assert!(location.longitude.is_none());
}

#[test]
fn snapshot_keeps_candidates_grouped_by_node_and_ipv4_nodes_first() {
    use easytier::proto::api::manage::{
        CollectNetworkInfoResponse, NetworkInstanceRunningInfo, NetworkInstanceRunningInfoMap,
    };
    let v6_only = easytier::proto::api::manage::MyNodeInfo {
        stun_info: Some(easytier::proto::common::StunInfo {
            public_ip: vec!["2001:4860:4860::8844".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let response = CollectNetworkInfoResponse {
        info: Some(NetworkInstanceRunningInfoMap {
            map: [
                (
                    "a".to_string(),
                    NetworkInstanceRunningInfo {
                        running: true,
                        my_node_info: Some(v6_only),
                        ..Default::default()
                    },
                ),
                (
                    "z".to_string(),
                    NetworkInstanceRunningInfo {
                        running: true,
                        my_node_info: Some(dual_stack_geo_node()),
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
        }),
    };
    let groups = ClientManager::snapshot_public_ips(&response).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(
        groups[0],
        vec![
            "1.1.1.1".parse::<std::net::IpAddr>().unwrap(),
            "2001:4860:4860::8888".parse().unwrap(),
        ]
    );
    assert_eq!(
        groups[1],
        vec!["2001:4860:4860::8844".parse::<std::net::IpAddr>().unwrap()]
    );
}

async fn wait_for_local_config_capability(
    mgr: &ClientManager,
    user_id: i32,
    machine_id: uuid::Uuid,
) {
    wait_for_condition(
        || async { mgr.supports_local_configs(&(user_id, machine_id)).await },
        Duration::from_secs(12),
    )
    .await;
}

fn local_config_instance_manager(config_dir: std::path::PathBuf) -> Arc<NativeInstanceManager> {
    Arc::new(native_instance_manager().with_config_path(Some(config_dir)))
}

async fn assert_no_managed_config_state(
    mgr: &ClientManager,
    user_id: i32,
    machine_id: uuid::Uuid,
    instance_id: uuid::Uuid,
) {
    assert!(
        mgr.db()
            .get_network_config((user_id, machine_id), &instance_id.to_string())
            .await
            .unwrap()
            .is_none(),
        "local-authority core must not create a managed config row"
    );
    assert!(
        mgr.db()
            .get_managed_config_revision((user_id, machine_id))
            .await
            .unwrap()
            .is_none(),
        "local-authority core must not create a managed config revision"
    );
}

#[tokio::test]
async fn legacy_delete_keeps_rejected_configs_in_web_storage() {
    use easytier_core::management::{ConfigFileControl, ConfigFilePermission};

    let (webhook_config, webhook_server, _) = test_webhook_config().await;
    let mut mgr = ClientManager::new(
        Db::memory_db().await,
        None,
        HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
        Arc::new(FeatureFlags::default()),
        webhook_config,
    );
    let server_addr = add_random_udp_listener(&mut mgr).await;
    let core_manager = Arc::new(native_instance_manager());
    let machine_id = uuid::Uuid::new_v4();
    let client = start_web_client_for_test(server_addr, machine_id, core_manager.clone()).await;
    let user_id = wait_for_validated_user(&mgr, machine_id).await;
    let identify = (user_id, machine_id);
    wait_for_condition(
        || async { mgr.handle_list_network_instance_ids(identify).await.is_ok() },
        Duration::from_secs(12),
    )
    .await;
    let removable_id = uuid::Uuid::new_v4();
    let protected_id = uuid::Uuid::new_v4();
    for id in [removable_id, protected_id] {
        let config = NetworkConfig {
            instance_id: Some(id.to_string()),
            network_name: Some(format!("legacy-delete-{id}")),
            networking_method: Some(NetworkingMethod::Standalone as i32),
            no_tun: Some(true),
            disable_p2p: Some(true),
            ..Default::default()
        };
        let permission = if id == protected_id {
            ConfigFilePermission::from(ConfigFilePermission::NO_DELETE)
        } else {
            ConfigFilePermission::default()
        };
        core_manager
            .run_network_instance(
                config.gen_config().unwrap(),
                ConfigFileControl::new(None, permission),
            )
            .unwrap();
        mgr.db()
            .insert_or_update_user_network_config(identify, id, config, ConfigSource::User)
            .await
            .unwrap();
    }
    assert!(
        mgr.handle_update_network_state(identify, protected_id, true)
            .await
            .is_err()
    );
    assert!(
        !mgr.db()
            .get_network_config(identify, &protected_id.to_string())
            .await
            .unwrap()
            .unwrap()
            .disabled
    );
    let error = mgr
        .handle_remove_network_instances(identify, vec![removable_id, protected_id])
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&protected_id.to_string()));
    assert!(!error.to_string().contains(&removable_id.to_string()));
    assert!(
        core_manager.instance(removable_id).is_none(),
        "removable instance survived: {error}"
    );
    assert!(core_manager.instance(protected_id).is_some());
    assert!(
        mgr.db()
            .get_network_config(identify, &removable_id.to_string())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        mgr.db()
            .get_network_config(identify, &protected_id.to_string())
            .await
            .unwrap()
            .is_some()
    );

    // A later heartbeat must not resurrect the successfully deleted row.
    let session = mgr.get_session_by_machine_id(user_id, &machine_id).unwrap();
    wait_for_condition(
        || async {
            session.get_heartbeat_req().await.is_some_and(|req| {
                req.running_network_instances.contains(&protected_id.into())
                    && !req.running_network_instances.contains(&removable_id.into())
            })
        },
        Duration::from_secs(12),
    )
    .await;
    assert!(core_manager.instance(removable_id).is_none());

    client.shutdown().await;
    core_manager.retain_network_instances(&[]).await.unwrap();
    webhook_server.abort();
}

#[tokio::test]
async fn local_config_delete_reports_protected_networks_and_removes_other_configs() {
    use easytier::common::config::{ConfigLoader as _, load_config_from_file};

    let (webhook_config, webhook_server, _) = test_webhook_config().await;
    let mut mgr = ClientManager::new(
        Db::memory_db().await,
        None,
        HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
        Arc::new(FeatureFlags::default()),
        webhook_config,
    );
    let server_addr = add_random_udp_listener(&mut mgr).await;
    let config_dir =
        std::env::temp_dir().join(format!("easytier-web-delete-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&config_dir).unwrap();
    let core_manager = local_config_instance_manager(config_dir.clone());
    let machine_id = uuid::Uuid::new_v4();
    let client = start_web_client_for_test(server_addr, machine_id, core_manager.clone()).await;
    let user_id = wait_for_validated_user(&mgr, machine_id).await;
    wait_for_local_config_capability(&mgr, user_id, machine_id).await;
    let identify = (user_id, machine_id);
    let running_id = uuid::Uuid::new_v4();
    let saved_id = uuid::Uuid::new_v4();
    let protected_id = uuid::Uuid::new_v4();
    let network = |id: uuid::Uuid| NetworkConfig {
        instance_id: Some(id.to_string()),
        network_name: Some(format!("delete-test-{id}")),
        networking_method: Some(NetworkingMethod::Standalone as i32),
        no_tun: Some(true),
        disable_p2p: Some(true),
        ..Default::default()
    };
    mgr.handle_run_network_instance(identify, network(running_id), true)
        .await
        .unwrap();
    mgr.handle_save_network_config(identify, saved_id, network(saved_id))
        .await
        .unwrap();

    let mut protected = network(protected_id);
    protected.network_secret =
        Some("${EASYTIER_WEB_DELETE_TEST_UNSET_SECRET:-protected-secret}".to_string());
    let protected = protected.gen_config().unwrap();
    protected.set_listeners(Vec::new());
    let protected_path = config_dir.join(format!("{protected_id}.toml"));
    std::fs::write(&protected_path, protected.dump()).unwrap();
    let (protected, control) = load_config_from_file(&protected_path, Some(&config_dir), false)
        .await
        .unwrap();
    assert!(control.is_read_only() && control.is_no_delete());
    core_manager
        .run_network_instance(protected, control)
        .unwrap();

    let error = mgr
        .handle_remove_network_instances(identify, vec![running_id, saved_id, protected_id])
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&protected_id.to_string()));
    assert!(core_manager.instance(running_id).is_none());
    assert!(!config_dir.join(format!("{running_id}.toml")).exists());
    assert!(!config_dir.join(format!("{saved_id}.toml")).exists());
    assert!(core_manager.instance(protected_id).is_some());
    assert!(protected_path.is_file());
    let ids = mgr
        .handle_list_network_instance_ids(identify)
        .await
        .unwrap();
    assert!(!ids.disabled_inst_ids.contains(&saved_id.into()));
    assert!(ids.running_inst_ids.contains(&protected_id.into()));

    // The config-only endpoint must enforce the same loader permissions.
    use easytier::proto::api::manage::RemoveNetworkInstanceConfigRequest;
    let response = mgr
        .get_rpc_client(identify)
        .unwrap()
        .remove_network_instance_config(
            Default::default(),
            RemoveNetworkInstanceConfigRequest {
                inst_ids: vec![protected_id.into()],

                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(response.remain_inst_ids, vec![protected_id.into()]);
    assert!(protected_path.is_file());

    client.shutdown().await;
    core_manager.retain_network_instances(&[]).await.unwrap();
    webhook_server.abort();
    std::fs::remove_dir_all(config_dir).unwrap();
}

#[tokio::test]
async fn local_config_core_ignores_full_managed_config_reconcile() {
    let (webhook_config, webhook_server, _) = test_webhook_config().await;
    let mut mgr = ClientManager::new(
        Db::memory_db().await,
        None,
        HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
        Arc::new(FeatureFlags::default()),
        webhook_config,
    );
    let config_server_addr = add_random_udp_listener(&mut mgr).await;
    let config_dir = std::env::temp_dir().join(format!(
        "easytier-web-local-config-full-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&config_dir).unwrap();

    let machine_id = uuid::Uuid::new_v4();
    let instance_id = uuid::Uuid::new_v4();
    let core_manager = local_config_instance_manager(config_dir.clone());
    let client =
        start_web_client_for_test(config_server_addr, machine_id, core_manager.clone()).await;
    let user_id = wait_for_validated_user(&mgr, machine_id).await;
    wait_for_local_config_capability(&mgr, user_id, machine_id).await;

    mgr.reconcile_managed_network_configs(
        user_id,
        machine_id,
        vec![managed_config(
            instance_id,
            initial_managed_network_config(instance_id),
        )],
        Some("local-full".to_string()),
        None,
    )
    .await
    .unwrap();

    assert_no_managed_config_state(&mgr, user_id, machine_id, instance_id).await;
    assert!(core_manager.instance(instance_id).is_none());
    assert!(!config_dir.join(format!("{instance_id}.toml")).exists());

    drop(client);
    webhook_server.abort();
    std::fs::remove_dir_all(config_dir).unwrap();
}

#[tokio::test]
async fn local_config_core_ignores_managed_config_patch() {
    let (webhook_config, webhook_server, _) = test_webhook_config().await;
    let mut mgr = ClientManager::new(
        Db::memory_db().await,
        None,
        HeartbeatPolicy::from_millis(0, 15_000).unwrap(),
        Arc::new(FeatureFlags::default()),
        webhook_config,
    );
    let config_server_addr = add_random_udp_listener(&mut mgr).await;
    let config_dir = std::env::temp_dir().join(format!(
        "easytier-web-local-config-patch-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&config_dir).unwrap();

    let machine_id = uuid::Uuid::new_v4();
    let instance_id = uuid::Uuid::new_v4();
    let core_manager = local_config_instance_manager(config_dir.clone());
    let client =
        start_web_client_for_test(config_server_addr, machine_id, core_manager.clone()).await;
    let user_id = wait_for_validated_user(&mgr, machine_id).await;
    wait_for_local_config_capability(&mgr, user_id, machine_id).await;

    mgr.patch_managed_network_configs(
        user_id,
        machine_id,
        vec![managed_config(
            instance_id,
            updated_managed_network_config(instance_id),
        )],
        Vec::new(),
        "local-patch".to_string(),
        "missing-local-base".to_string(),
    )
    .await
    .unwrap();

    assert_no_managed_config_state(&mgr, user_id, machine_id, instance_id).await;
    assert!(core_manager.instance(instance_id).is_none());
    assert!(!config_dir.join(format!("{instance_id}.toml")).exists());

    drop(client);
    webhook_server.abort();
    std::fs::remove_dir_all(config_dir).unwrap();
}
