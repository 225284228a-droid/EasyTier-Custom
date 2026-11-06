//! Custom (non-upstream) tests for the central network service.
//!
//! These tests build on the upstream `service.rs` tests module but are not
//! part of the upstream file, so they live here to keep that file close to
//! its upstream shape and reduce the merge surface.

use crate::central_network::{
    advanced::AdvancedChange, mesh_network::Preset, network_acl::NetworkAcl,
};

use super::*;

#[tokio::test]
async fn local_config_members_are_accepted_offline_with_visible_pending_delivery() {
    use easytier_core::management::remote_client::Storage as _;
    let (service, user_id, _) = service().await;
    let device_id = Uuid::new_v4();
    register_device(&service, user_id, device_id).await;
    service
        .db
        .store_local_config_mode(user_id, device_id, Uuid::new_v4(), 1, true)
        .await
        .unwrap();
    let network = service
        .create_network(user_id, standalone_settings(), None)
        .await
        .unwrap();
    let network_id = Uuid::parse_str(&network.network_id).unwrap();
    service
        .add_members(
            user_id,
            network_id,
            AddMembersReq {
                device_ids: vec![device_id.to_string()],
                temporary: true,
                ttl_seconds: None,
            },
        )
        .await
        .unwrap();
    let members = service
        .list_members(user_id, network_id)
        .await
        .unwrap()
        .members;
    assert_eq!(members.len(), 1);
    assert!(members[0].error_msg.as_ref().unwrap().contains("offline"));
    assert!(
        service
            .db
            .get_network_config((user_id, device_id), &network_id.to_string())
            .await
            .unwrap()
            .is_none()
    );
    service.delete_network(user_id, network_id).await.unwrap();
    let rows = service
        .db
        .project_central_local_configs(user_id, device_id, &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].desired_config.is_none());
}

#[tokio::test]
async fn whitespace_only_secrets_are_rejected_and_custom_secrets_trimmed() {
    let (service, user_id, _) = service().await;
    let error = service
        .create_network(user_id, standalone_settings(), Some(" \t ".to_owned()))
        .await
        .unwrap_err();
    assert!(matches!(error, CentralNetworkServiceError::Invalid(_)));

    let network = service
        .create_network(
            user_id,
            standalone_settings(),
            Some("  custom  ".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(network.network_secret, "custom");
    let network_id = Uuid::parse_str(&network.network_id).unwrap();

    let error = service
        .update_network(
            user_id,
            network_id,
            UpdateNetworkReq {
                settings: standalone_settings(),
                network_secret: Some("   ".to_owned()),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CentralNetworkServiceError::Invalid(_)));
    let unchanged = service.get_network(user_id, network_id).await.unwrap();
    assert_eq!(unchanged.network_secret, "custom");
}

#[tokio::test]
async fn passwordless_secure_networks_publish_restartable_temporary_configs() {
    use easytier::common::config::NetworkConfigExt as _;
    use easytier_core::management::remote_client::{PersistentConfig as _, Storage as _};

    for create_without_password in [true, false] {
        let (service, user_id, _) = service().await;
        // Permanent members run the network from their own files; the
        // persisted passwordless identity is covered by Core's tests.
        let temporary_device = Uuid::new_v4();
        register_device(&service, user_id, temporary_device).await;
        let network = service
            .create_network(
                user_id,
                standalone_settings(),
                create_without_password.then(String::new),
            )
            .await
            .unwrap();
        let network_id = Uuid::parse_str(&network.network_id).unwrap();
        service
            .add_members(
                user_id,
                network_id,
                AddMembersReq {
                    device_ids: vec![temporary_device.to_string()],
                    temporary: true,
                    ttl_seconds: Some(300),
                },
            )
            .await
            .unwrap();
        if !create_without_password {
            service
                .update_network(
                    user_id,
                    network_id,
                    UpdateNetworkReq {
                        settings: standalone_settings(),
                        network_secret: Some(String::new()),
                    },
                )
                .await
                .unwrap();
        }
        let saved_network = service.get_network(user_id, network_id).await.unwrap();
        assert!(saved_network.secure_mode);
        assert!(saved_network.network_secret.is_empty());

        let saved_temporary = service
            .db
            .get_network_config((user_id, temporary_device), &network_id.to_string())
            .await
            .unwrap()
            .unwrap();
        let temporary_config = saved_temporary
            .get_network_config()
            .unwrap()
            .gen_config()
            .unwrap();
        let restored_temporary = TomlConfigLoader::new_from_str(&temporary_config.dump()).unwrap();
        assert_eq!(
            restored_temporary.get_network_identity().network_secret,
            None
        );
        assert!(restored_temporary.get_managed_credentials().is_empty());
        easytier_core::instance::CoreInstanceConfig::from_toml(&restored_temporary).unwrap();
    }
}

#[tokio::test]
async fn offline_official_nodes_get_core_changes_forced_into_their_stored_config() {
    use easytier_core::management::remote_client::{
        ListNetworkProps, PersistentConfig as _, Storage as _,
    };
    let (service, user_id, _) = service().await;
    let device_id = Uuid::new_v4();
    register_device(&service, user_id, device_id).await;
    let instance_id = Uuid::new_v4();
    service
        .db
        .insert_or_update_user_network_config(
            (user_id, device_id),
            instance_id,
            NetworkConfig {
                instance_id: Some(instance_id.to_string()),
                network_name: Some("office".into()),
                network_secret: Some("old".into()),
                hostname: Some("official-node".into()),
                ..Default::default()
            },
            easytier::common::config::ConfigSource::Web,
        )
        .await
        .unwrap();
    service
        .db
        .update_network_config_state((user_id, device_id), instance_id, true)
        .await
        .unwrap();

    let results = service
        .apply_core_change(
            user_id,
            "office",
            CoreChange {
                network_secret: Some("new".into()),
                display_name: Some("Head office".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, core_sync::MemberSyncStatus::Pending);

    let stored = service
        .db
        .list_network_configs((user_id, device_id), ListNetworkProps::All)
        .await
        .unwrap()
        .remove(0);
    let config = stored.get_network_config().unwrap();
    assert_eq!(config.network_secret.as_deref(), Some("new"));
    assert_eq!(config.hostname.as_deref(), Some("official-node"));
    // Official cores cannot keep a separate display name.
    assert_eq!(config.instance_name, None);
    assert!(stored.disabled, "a stopped network stays stopped");
}

#[tokio::test]
async fn subnet_moves_keep_host_parts_and_leave_dhcp_members_alone() {
    use easytier_core::management::remote_client::{
        ListNetworkProps, PersistentConfig as _, Storage as _,
    };
    let (service, user_id, _) = service().await;
    // Temporary devices with their own subnet follow the network's.
    let temporary_devices = [Uuid::new_v4(), Uuid::new_v4()];
    for device in temporary_devices {
        register_device(&service, user_id, device).await;
    }
    let network = service
        .create_network(user_id, standalone_settings(), None)
        .await
        .unwrap();
    let network_id = Uuid::parse_str(&network.network_id).unwrap();
    service
        .add_members(
            user_id,
            network_id,
            AddMembersReq {
                device_ids: temporary_devices.iter().map(ToString::to_string).collect(),
                temporary: true,
                ttl_seconds: None,
            },
        )
        .await
        .unwrap();
    let before: Vec<_> = service
        .load(user_id, network_id)
        .await
        .unwrap()
        .members
        .into_iter()
        .map(|member| member.allocated_ipv4.unwrap())
        .collect();

    let mut members = Vec::new();
    for (address, dhcp) in [(Some("10.88.0.5"), false), (None, true)] {
        let device_id = Uuid::new_v4();
        register_device(&service, user_id, device_id).await;
        let instance_id = Uuid::new_v4();
        service
            .db
            .insert_or_update_user_network_config(
                (user_id, device_id),
                instance_id,
                NetworkConfig {
                    instance_id: Some(instance_id.to_string()),
                    network_name: Some("engineering".into()),
                    network_secret: Some("secret".into()),
                    dhcp: Some(dhcp),
                    virtual_ipv4: address.map(str::to_owned),
                    network_length: address.map(|_| 24),
                    ..Default::default()
                },
                easytier::common::config::ConfigSource::Web,
            )
            .await
            .unwrap();
        members.push(device_id);
    }

    let results = service
        .apply_core_change(
            user_id,
            "engineering",
            CoreChange {
                virtual_subnet: Some("10.20.30.0/24".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut stored = Vec::new();
    for device_id in &members {
        stored.push(
            service
                .db
                .list_network_configs((user_id, *device_id), ListNetworkProps::All)
                .await
                .unwrap()
                .remove(0)
                .get_network_config()
                .unwrap(),
        );
    }
    assert_eq!(stored[0].virtual_ipv4.as_deref(), Some("10.20.30.5"));
    assert_eq!(stored[0].network_length, Some(24));
    assert_eq!(stored[1].virtual_ipv4, None);
    assert_eq!(stored[1].dhcp, Some(true));
    assert!(
        results
            .iter()
            .any(|result| result.device_id == members[1].to_string()
                && result.status == core_sync::MemberSyncStatus::Unchanged)
    );

    let after = service.load(user_id, network_id).await.unwrap();
    assert_eq!(after.virtual_cidr.as_deref(), Some("10.20.30.0/24"));
    let after: Vec<_> = after
        .members
        .into_iter()
        .map(|member| member.allocated_ipv4.unwrap())
        .collect();
    for (before, after) in before.iter().zip(&after) {
        assert_eq!(before.rsplit('.').next(), after.rsplit('.').next());
        assert!(after.starts_with("10.20.30."), "{after}");
    }
}

#[tokio::test]
async fn permanent_members_are_rejected_and_temporary_ones_get_exclusive_credentials() {
    let (service, user_id, _) = service().await;
    let permanent_device = Uuid::new_v4();
    let temporary_device = Uuid::new_v4();
    register_device(&service, user_id, permanent_device).await;
    register_device(&service, user_id, temporary_device).await;
    let network = service
        .create_network(user_id, standalone_settings(), None)
        .await
        .unwrap();
    let network_id = Uuid::parse_str(&network.network_id).unwrap();

    let error = service
        .add_members(
            user_id,
            network_id,
            AddMembersReq {
                device_ids: vec![permanent_device.to_string()],
                temporary: false,
                ttl_seconds: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CentralNetworkServiceError::Invalid(_)));
    service
        .add_members(
            user_id,
            network_id,
            AddMembersReq {
                device_ids: vec![temporary_device.to_string()],
                temporary: true,
                ttl_seconds: Some(300),
            },
        )
        .await
        .unwrap();

    let intent = service
        .db
        .load_central_network_intent(user_id, network_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(intent.members.len(), 1);
    let temporary = &intent.members[0];
    let credential = intent
        .credentials
        .iter()
        .find(|credential| Some(&credential.id) == temporary.credential_id.as_ref())
        .unwrap();
    assert!(!credential.grant.reusable);
    assert_eq!(
        credential.grant.acl_groups,
        vec![crate::central_network::model::member_group_name(
            temporary.id
        )]
    );
}

#[tokio::test]
async fn one_users_mutation_lock_does_not_block_another_user() {
    let (service, user_a, user_b) = service().await;
    let device_b = Uuid::new_v4();
    register_device(&service, user_b, device_b).await;
    let network_b = service
        .create_network(user_b, standalone_settings(), None)
        .await
        .unwrap();
    let network_b_id = Uuid::parse_str(&network_b.network_id).unwrap();
    service
        .add_members(
            user_b,
            network_b_id,
            AddMembersReq {
                device_ids: vec![device_b.to_string()],
                temporary: true,
                ttl_seconds: None,
            },
        )
        .await
        .unwrap();

    // Mutations are per user: a lock user A holds (e.g. while deleting a
    // device of theirs) must not serialize user B's changes behind it.
    let guard_a = service.lock_mutations(user_a).await;
    let waiting_service = service.clone();
    let update_b = tokio::spawn(async move {
        waiting_service
            .update_member(
                user_b,
                network_b_id,
                device_b,
                UpdateMemberReq {
                    hostname_override: Some("unblocked".into()),
                    ..Default::default()
                },
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), update_b)
        .await
        .expect("user B's mutation must not wait for user A's lock")
        .unwrap()
        .unwrap();
    assert_eq!(
        service.load(user_b, network_b_id).await.unwrap().members[0]
            .hostname
            .as_deref(),
        Some("unblocked")
    );
    drop(guard_a);
}

#[tokio::test]
async fn removing_member_revokes_its_credential_and_clears_its_configs() {
    use easytier_core::management::remote_client::{ListNetworkProps, Storage as _};
    let (service, user_id, _) = service().await;
    let temporary = Uuid::new_v4();
    register_device(&service, user_id, temporary).await;
    let network = service
        .create_network(user_id, standalone_settings(), None)
        .await
        .unwrap();
    let network_id = Uuid::parse_str(&network.network_id).unwrap();
    service
        .add_members(
            user_id,
            network_id,
            AddMembersReq {
                device_ids: vec![temporary.to_string()],
                temporary: true,
                ttl_seconds: None,
            },
        )
        .await
        .unwrap();
    let intent = service.load(user_id, network_id).await.unwrap();
    assert_eq!(intent.credentials.len(), 1);
    service
        .remove_member(user_id, network_id, temporary)
        .await
        .unwrap();
    let intent = service.load(user_id, network_id).await.unwrap();
    assert!(intent.credentials.is_empty());
    assert!(
        service
            .db
            .list_network_configs((user_id, temporary), ListNetworkProps::All)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn official_nodes_get_private_mode_on_unless_they_chose() {
    let (service, user_id, _) = service().await;
    let (unset, _) = official_member(&service, user_id, "office").await;
    let (chosen, chosen_instance) = official_member(&service, user_id, "office").await;
    let (mut config, _) = stored_config(&service, user_id, chosen).await;
    config.enable_private_mode = Some(false);
    {
        use easytier_core::management::remote_client::Storage as _;
        service
            .db
            .insert_or_update_user_network_config(
                (user_id, chosen),
                chosen_instance,
                config,
                easytier::common::config::ConfigSource::Web,
            )
            .await
            .unwrap();
    }

    core_sync::observe_user(&service.db, &service.client_manager, user_id).await;
    let (config, _) = stored_config(&service, user_id, unset).await;
    assert_eq!(config.enable_private_mode, Some(true));
    let (config, _) = stored_config(&service, user_id, chosen).await;
    assert_eq!(
        config.enable_private_mode,
        Some(false),
        "an explicit choice stays"
    );
}

async fn official_member(
    service: &CentralNetworkService,
    user_id: i32,
    network_name: &str,
) -> (Uuid, Uuid) {
    use easytier_core::management::remote_client::Storage as _;
    let device_id = Uuid::new_v4();
    register_device(service, user_id, device_id).await;
    let instance_id = Uuid::new_v4();
    service
        .db
        .insert_or_update_user_network_config(
            (user_id, device_id),
            instance_id,
            NetworkConfig {
                instance_id: Some(instance_id.to_string()),
                network_name: Some(network_name.into()),
                network_secret: Some("s1".into()),
                hostname: Some("official-node".into()),
                latency_first: Some(false),
                ..Default::default()
            },
            easytier::common::config::ConfigSource::Web,
        )
        .await
        .unwrap();
    (device_id, instance_id)
}

async fn stored_config(
    service: &CentralNetworkService,
    user_id: i32,
    device_id: Uuid,
) -> (NetworkConfig, bool) {
    use easytier_core::management::remote_client::{
        ListNetworkProps, PersistentConfig as _, Storage as _,
    };
    let stored = service
        .db
        .list_network_configs((user_id, device_id), ListNetworkProps::All)
        .await
        .unwrap()
        .remove(0);
    (stored.get_network_config().unwrap(), stored.disabled)
}

/// A revision node that is offline now, last seen holding `network_name`.
async fn offline_revision_member(
    service: &CentralNetworkService,
    user_id: i32,
    network_name: &str,
) -> (Uuid, Uuid) {
    let device_id = Uuid::new_v4();
    register_device(service, user_id, device_id).await;
    service
        .db
        .store_local_config_mode(user_id, device_id, Uuid::new_v4(), 1, true)
        .await
        .unwrap();
    let instance_id = Uuid::new_v4();
    let config = NetworkConfig {
        network_name: Some(network_name.into()),
        network_secret: Some("s1".into()),
        mtu: Some(1300),
        ..Default::default()
    };
    service
        .db
        .replace_mesh_observations(
            user_id,
            device_id,
            &[crate::db::MeshObservation {
                device_id: device_id.to_string(),
                instance_id: instance_id.to_string(),
                network_name: network_name.into(),
                core: serde_json::to_string(&CoreSettings::of(&config)).unwrap(),
                sync_at: Some(100),
                credential: false,
                advanced_sync_at: None,
                advanced: serde_json::to_string(&crate::central_network::advanced::values_of(
                    &config,
                ))
                .unwrap(),
                acl: "{}".to_owned(),
            }],
        )
        .await
        .unwrap();
    (device_id, instance_id)
}

#[tokio::test]
async fn a_network_subnet_is_optional_and_otherwise_read_from_its_members() {
    let (service, user_id, _) = service().await;
    let request = |name: &str, subnet: Option<&str>| CreateMeshNetworkReq {
        network_name: name.into(),
        display_name: None,
        network_secret: None,
        secure_mode: false,
        preset: None,
        devices: Vec::new(),
        virtual_subnet: subnet.map(str::to_owned),
    };
    assert!(
        service
            .create_mesh_network(user_id, request("bad", Some("10.20.30.0")))
            .await
            .is_err(),
        "a subnet needs its prefix length"
    );
    service
        .create_mesh_network(user_id, request("lab", Some("10.20.30.9/24")))
        .await
        .unwrap();
    let lab = service.get_mesh_network(user_id, "lab").await.unwrap();
    assert_eq!(lab.core.virtual_ipv4.as_deref(), Some("10.20.30.0/24"));
    service
        .create_mesh_network(user_id, request("default", None))
        .await
        .unwrap();
    let default = service.get_mesh_network(user_id, "default").await.unwrap();
    assert_eq!(default.core.virtual_ipv4, None, "empty means the default");

    // A network found on devices: its static members say where it is,
    // else its DHCP members' configured subnet does.
    let observed = |instance: &str, core: CoreSettings| crate::db::MeshObservation {
        device_id: Uuid::new_v4().to_string(),
        instance_id: instance.to_owned(),
        network_name: core.network_name.clone(),
        core: serde_json::to_string(&core).unwrap(),
        sync_at: None,
        credential: false,
        advanced: "{}".to_owned(),
        advanced_sync_at: None,
        acl: "{}".to_owned(),
    };
    let core = |name: &str, address: Option<&str>, range: Option<&str>| CoreSettings {
        network_name: name.into(),
        network_secret: Some("secret".into()),
        virtual_ipv4: address.map(str::to_owned),
        dhcp_ipv4_cidr: range.map(str::to_owned),
        ..Default::default()
    };
    for (instance, member) in [
        ("a", core("found", Some("10.9.9.5/24"), None)),
        ("b", core("found", Some("10.9.9.6/24"), None)),
        ("c", core("found", None, Some("10.7.7.0/24"))),
        ("d", core("dhcp", None, Some("10.7.7.0/24"))),
        ("e", core("plain", None, None)),
    ] {
        let observation = observed(instance, member);
        let device_id = Uuid::parse_str(&observation.device_id).unwrap();
        register_device(&service, user_id, device_id).await;
        service
            .db
            .replace_mesh_observations(user_id, device_id, &[observation])
            .await
            .unwrap();
    }
    let subnet = |name: &'static str| {
        let service = &service;
        async move {
            service
                .get_mesh_network(user_id, name)
                .await
                .unwrap()
                .core
                .virtual_ipv4
        }
    };
    assert_eq!(subnet("found").await.as_deref(), Some("10.9.9.0/24"));
    assert_eq!(subnet("dhcp").await.as_deref(), Some("10.7.7.0/24"));
    assert_eq!(subnet("plain").await, None);
}

#[tokio::test]
async fn a_network_needs_only_a_name_and_devices_join_with_its_preset() {
    let (service, user_id, _) = service().await;
    let results = service
        .create_mesh_network(
            user_id,
            CreateMeshNetworkReq {
                network_name: " lab ".into(),
                display_name: None,
                network_secret: None,
                secure_mode: false,
                preset: None,
                devices: Vec::new(),
                virtual_subnet: None,
            },
        )
        .await
        .unwrap();
    assert!(results.is_empty());
    let network = service.get_mesh_network(user_id, "lab").await.unwrap();
    assert!(
        network
            .core
            .network_secret
            .as_deref()
            .is_some_and(|secret| !secret.is_empty()),
        "a secret is generated"
    );
    assert_eq!(
        network.preset.listeners,
        ["tcp://0.0.0.0:11010", "udp://0.0.0.0:11010"],
        "new networks do not listen for WireGuard"
    );
    assert_eq!(service.list_mesh_networks(user_id).await.unwrap().len(), 1);
    let duplicate = service
        .create_mesh_network(
            user_id,
            CreateMeshNetworkReq {
                network_name: "lab".into(),
                display_name: None,
                network_secret: None,
                secure_mode: false,
                preset: None,
                devices: Vec::new(),
                virtual_subnet: None,
            },
        )
        .await;
    assert!(duplicate.is_err());

    let mut preset = network.preset.clone();
    preset.peers = vec!["tcp://seed.example:11010".into()];
    preset
        .advanced
        .insert("latency_first".into(), serde_json::json!(true));
    service
        .save_mesh_preset(user_id, "lab", preset)
        .await
        .unwrap();

    let device_id = Uuid::new_v4();
    register_device(&service, user_id, device_id).await;
    let joined = service
        .join_mesh_devices(user_id, "lab", &[device_id])
        .await
        .unwrap();
    // Official nodes receive it when they connect.
    assert_eq!(joined[0].status, core_sync::MemberSyncStatus::Pending);
    let (config, disabled) = stored_config(&service, user_id, device_id).await;
    assert_eq!(config.network_name.as_deref(), Some("lab"));
    assert_eq!(config.network_secret, network.core.network_secret);
    assert_eq!(config.peer_urls, ["tcp://seed.example:11010"]);
    assert_eq!(config.latency_first, Some(true));
    assert_eq!(config.dhcp, Some(true));
    assert!(!disabled);
    // An existing member is left alone.
    let again = service
        .join_mesh_devices(user_id, "lab", &[device_id])
        .await
        .unwrap();
    assert_eq!(again[0].status, core_sync::MemberSyncStatus::Unchanged);
}

#[tokio::test]
async fn advanced_edits_skip_locked_members_and_replay_to_offline_ones() {
    let (service, user_id, _) = service().await;
    let (official, official_instance) = official_member(&service, user_id, "office").await;
    let (offline, offline_instance) = offline_revision_member(&service, user_id, "office").await;
    let (locked, locked_instance) = official_member(&service, user_id, "office").await;
    service
        .set_mesh_lock(user_id, locked, locked_instance, true)
        .await
        .unwrap();

    let change: AdvancedChange = [
        ("latency_first".to_owned(), serde_json::json!(true)),
        ("mtu".to_owned(), serde_json::json!(1200)),
    ]
    .into();
    let results = service
        .apply_mesh_advanced(user_id, "office", change)
        .await
        .unwrap();
    let status = |instance: Uuid| {
        results
            .iter()
            .find(|result| result.instance_id == instance.to_string())
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(
        status(official_instance),
        core_sync::MemberSyncStatus::Pending
    );
    assert_eq!(
        status(offline_instance),
        core_sync::MemberSyncStatus::Pending
    );
    assert_eq!(status(locked_instance), core_sync::MemberSyncStatus::Locked);

    let (config, _) = stored_config(&service, user_id, official).await;
    assert_eq!(config.latency_first, Some(true));
    assert_eq!(config.mtu, Some(1200));
    assert_eq!(config.hostname.as_deref(), Some("official-node"));
    let (untouched, _) = stored_config(&service, user_id, locked).await;
    assert_eq!(untouched.latency_first, Some(false));

    let replays = service
        .db
        .list_mesh_advanced_replays(user_id)
        .await
        .unwrap();
    assert_eq!(replays.len(), 1);
    assert_eq!(replays[0].device_id, offline.to_string());
    let missed: serde_json::Value = serde_json::from_str(&replays[0].change).unwrap();
    assert_eq!(
        missed,
        serde_json::json!({ "latency_first": true, "mtu": 1200 })
    );
    assert!(replays[0].target_sync_at > 0);

    let members = service.mesh_members(user_id).await.unwrap();
    let member = |instance: Uuid| {
        members
            .iter()
            .find(|member| member.instance_id == instance.to_string())
            .unwrap()
    };
    assert!(member(offline_instance).pending);
    assert!(member(locked_instance).locked);
}

#[tokio::test]
async fn network_acl_is_written_to_members_and_rebuilt_from_them() {
    use crate::central_network::{
        model::{AclAction, AclProtocol, AclProtocolTarget},
        network_acl::{AclPeer, AclTarget, NetworkAclGroup, NetworkAclRule},
    };

    let (service, user_id, _) = service().await;
    let (admin, admin_instance) = official_member(&service, user_id, "office").await;
    let (server, _) = official_member(&service, user_id, "office").await;
    let acl = NetworkAcl {
        default_action: AclAction::Deny,
        groups: vec![NetworkAclGroup {
            name: "admins".to_owned(),
            secret: String::new(),
            members: [admin_instance.to_string()].into(),
        }],
        rules: vec![NetworkAclRule {
            id: "ssh".to_owned(),
            name: "Admins use SSH".to_owned(),
            enabled: true,
            action: AclAction::Allow,
            sources: vec![AclPeer::Group {
                name: "admins".to_owned(),
            }],
            destinations: vec![AclTarget::All],
            protocols: vec![AclProtocolTarget {
                protocol: AclProtocol::Tcp,
                ports: vec!["22".to_owned()],
                stateful: true,
            }],
        }],
        device_secrets: Default::default(),
    };
    service
        .set_mesh_network_acl(user_id, "office", Some(acl.clone()))
        .await
        .unwrap();

    let group_of = |config: &easytier::proto::api::manage::NetworkConfig| {
        config.acl.clone().unwrap().acl_v1.unwrap().group.unwrap()
    };
    let (admin_config, _) = stored_config(&service, user_id, admin).await;
    let (server_config, _) = stored_config(&service, user_id, server).await;
    assert_eq!(group_of(&admin_config).members, ["admins"]);
    assert!(group_of(&server_config).members.is_empty());
    let secret = &group_of(&server_config).declares[0].group_secret;
    assert!(!secret.is_empty());
    assert_eq!(&group_of(&admin_config).declares[0].group_secret, secret);

    let view = service
        .get_mesh_network_acl(user_id, "office")
        .await
        .unwrap();
    assert_eq!(view.acl, Some(acl.clone().normalized()));
    assert!(view.members.iter().all(|member| member.in_sync));

    // Saving the rebuilt policy keeps the generated secret.
    service
        .set_mesh_network_acl(user_id, "office", view.acl)
        .await
        .unwrap();
    let (server_config, _) = stored_config(&service, user_id, server).await;
    assert_eq!(&group_of(&server_config).declares[0].group_secret, secret);

    service
        .set_mesh_network_acl(user_id, "office", None)
        .await
        .unwrap();
    let (admin_config, _) = stored_config(&service, user_id, admin).await;
    assert_eq!(admin_config.acl, None);
    assert_eq!(
        service
            .get_mesh_network_acl(user_id, "office")
            .await
            .unwrap()
            .acl,
        None
    );
}

#[tokio::test]
async fn deleting_a_network_pauses_members_and_keeps_their_configs() {
    let (service, user_id, _) = service().await;
    let (official, official_instance) = official_member(&service, user_id, "office").await;
    let (offline, offline_instance) = offline_revision_member(&service, user_id, "office").await;
    // A lock guards settings only, so it does not keep a member running.
    service
        .set_mesh_lock(user_id, official, official_instance, true)
        .await
        .unwrap();
    service
        .save_mesh_preset(user_id, "office", Preset::new(None))
        .await
        .unwrap();

    let results = service
        .delete_mesh_network(user_id, "office")
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    let (config, disabled) = stored_config(&service, user_id, official).await;
    assert_eq!(config.network_name.as_deref(), Some("office"));
    assert!(disabled, "the official member is paused");
    assert_eq!(
        service.db.list_mesh_pause_replays(user_id).await.unwrap(),
        [(offline.to_string(), offline_instance.to_string())]
    );
    assert!(
        service
            .list_mesh_networks(user_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn only_devices_unknown_when_auto_join_starts_may_join_on_their_own() {
    let (service, user_id, _) = service().await;
    official_member(&service, user_id, "office").await;
    let known = Uuid::new_v4();
    register_device(&service, user_id, known).await;
    service
        .set_mesh_auto_join(user_id, "office", true)
        .await
        .unwrap();
    let joined = service
        .db
        .mesh_auto_joined(user_id, "office")
        .await
        .unwrap();
    assert!(joined.contains(&known.to_string()));
    assert!(
        service
            .get_mesh_network(user_id, "office")
            .await
            .unwrap()
            .auto_join
    );
    let newcomer = Uuid::new_v4();
    register_device(&service, user_id, newcomer).await;
    let joined = service
        .db
        .mesh_auto_joined(user_id, "office")
        .await
        .unwrap();
    assert!(!joined.contains(&newcomer.to_string()));
}

#[tokio::test]
async fn core_changes_follow_the_consoles_own_record_through_a_rename() {
    let (service, user_id, _) = service().await;
    service
        .create_mesh_network(
            user_id,
            CreateMeshNetworkReq {
                network_name: "lab".into(),
                display_name: None,
                network_secret: Some("s1".into()),
                secure_mode: false,
                preset: None,
                devices: Vec::new(),
                virtual_subnet: None,
            },
        )
        .await
        .unwrap();
    service
        .apply_core_change(
            user_id,
            "lab",
            CoreChange {
                network_name: Some("lab2".into()),
                network_secret: Some("s2".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(service.get_mesh_network(user_id, "lab").await.is_err());
    let renamed = service.get_mesh_network(user_id, "lab2").await.unwrap();
    assert_eq!(renamed.core.network_secret.as_deref(), Some("s2"));
}

#[tokio::test]
async fn a_device_joins_a_network_once_as_member_or_temporary_device() {
    let (service, user_id, _) = service().await;
    let (permanent, _) = offline_revision_member(&service, user_id, "engineering").await;
    let newcomer = Uuid::new_v4();
    register_device(&service, user_id, newcomer).await;
    let network = service
        .create_network(user_id, standalone_settings(), None)
        .await
        .unwrap();
    let network_id = Uuid::parse_str(&network.network_id).unwrap();
    let add = |device: Uuid| AddMembersReq {
        device_ids: vec![device.to_string()],
        temporary: true,
        ttl_seconds: None,
    };
    let duplicate = service
        .add_members(user_id, network_id, add(permanent))
        .await;
    assert!(
        matches!(duplicate, Err(CentralNetworkServiceError::Conflict(_))),
        "a device running the network itself is already a member: {duplicate:?}"
    );
    service
        .add_members(user_id, network_id, add(newcomer))
        .await
        .unwrap();
    let again = service
        .add_members(user_id, network_id, add(newcomer))
        .await;
    assert!(matches!(
        again,
        Err(CentralNetworkServiceError::Conflict(_))
    ));
    let twice = service
        .add_members(
            user_id,
            network_id,
            AddMembersReq {
                device_ids: vec![Uuid::new_v4().to_string(), newcomer.to_string()],
                temporary: true,
                ttl_seconds: None,
            },
        )
        .await;
    assert!(twice.is_err(), "a rejected batch adds nobody");
    assert_eq!(
        service
            .load(user_id, network_id)
            .await
            .unwrap()
            .members
            .len(),
        1
    );
}

#[tokio::test]
async fn a_zero_ttl_issues_credentials_that_never_expire() {
    let (service, user_id, _) = service().await;
    let temporary = Uuid::new_v4();
    register_device(&service, user_id, temporary).await;
    let network = service
        .create_network(user_id, standalone_settings(), None)
        .await
        .unwrap();
    let network_id = Uuid::parse_str(&network.network_id).unwrap();
    service
        .add_members(
            user_id,
            network_id,
            AddMembersReq {
                device_ids: vec![temporary.to_string()],
                temporary: true,
                ttl_seconds: Some(0),
            },
        )
        .await
        .unwrap();
    let shared = service
        .generate_credential(
            user_id,
            network_id,
            GenerateCredentialReq {
                ttl_seconds: 0,
                credential_id: None,
                reusable: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(shared.expiry_unix, PERMANENT_CREDENTIAL_EXPIRY_UNIX);
    let intent = service.load(user_id, network_id).await.unwrap();
    assert_eq!(intent.credentials.len(), 2);
    assert!(
        intent
            .credentials
            .iter()
            .all(|credential| credential.expiry_unix == PERMANENT_CREDENTIAL_EXPIRY_UNIX)
    );
    assert!(matches!(
        service
            .generate_credential(
                user_id,
                network_id,
                GenerateCredentialReq {
                    ttl_seconds: MAX_CREDENTIAL_TTL_SECONDS + 1,
                    credential_id: None,
                    reusable: true,
                },
            )
            .await,
        Err(CentralNetworkServiceError::Invalid(_))
    ));
}
