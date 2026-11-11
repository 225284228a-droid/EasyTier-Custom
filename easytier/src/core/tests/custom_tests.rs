use super::*;
use crate::instance::config_dir_startup::config_document_identity;

fn startup_fixture_config(id: uuid::Uuid) -> String {
    format!(
        "instance_id = '{id}'\nlisteners = []\nstun_servers = []\nstun_servers_v6 = []\ntcp_stun_servers = []\n[flags]\nno_tun = true\nbind_device = false\nenable_ipv6 = false\ndisable_upnp = true\n"
    )
}

#[tokio::test]
async fn config_dir_startup_isolates_invalid_and_preserves_stopped_state() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path().to_path_buf();
    let invalid = uuid::Uuid::new_v4();
    let stopped = uuid::Uuid::new_v4();
    let active = uuid::Uuid::new_v4();
    let removed = uuid::Uuid::new_v4();
    let removed_enabled = uuid::Uuid::new_v4();
    for id in [invalid, stopped, active] {
        // A file that cannot be loaded is isolated from the others.
        let raw = if id == invalid {
            vec![0xff, 0xfe]
        } else {
            startup_fixture_config(id).into_bytes()
        };
        std::fs::write(directory.join(format!("{id}.toml")), raw).unwrap();
    }
    let state = InstanceStateStore::new(Some(&directory));
    for id in [invalid, stopped, removed] {
        state.set_enabled(id, false).unwrap();
    }
    state.set_enabled(removed_enabled, true).unwrap();
    let mut startup = prepare_startup_configs(&[], Some(&directory), true)
        .await
        .unwrap();
    assert_eq!(startup.directory.config_dir_file_count, 3);
    assert_eq!(startup.files.len(), 2);
    let manager = native_cli_instance_manager(false, 0)
        .unwrap()
        .with_config_path(Some(directory.clone()));
    assert!(
        !start_prepared_configs(
            &mut startup,
            &manager,
            &state,
            &NetworkOptions::default(),
            false,
        )
        .unwrap()
    );
    assert!(manager.instance(active).is_some());
    assert!(manager.instance(invalid).is_none());
    assert!(manager.instance(stopped).is_none());
    assert!(manager.managed_config_ids().contains(&stopped));
    startup.reconcile_state(&state);
    let reloaded = InstanceStateStore::new(Some(&directory));
    assert_eq!(reloaded.enabled_state(&active), Some(true));
    assert_eq!(reloaded.enabled_state(&removed_enabled), None);
    assert!(!state.is_enabled(&invalid));
    assert!(!state.is_enabled(&stopped));
    assert_eq!(state.enabled_state(&removed), None);
    manager.delete_network_instances([active]).await.unwrap();

    // A malformed arbitrary filename cannot prove which old UUID it
    // belonged to; retaining old state is safer than silently enabling it.
    state.set_enabled(removed, false).unwrap();
    std::fs::write(directory.join("unknown.toml"), "[flags\ninvalid").unwrap();
    let startup = prepare_startup_configs(&[], Some(&directory), true)
        .await
        .unwrap();
    assert!(startup.directory.unidentified_config_dir_entry);
    startup.reconcile_state(&state);
    assert!(!state.is_enabled(&removed));
}

#[tokio::test]
async fn config_dir_startup_skips_all_duplicate_owners_including_invalid_files() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path().to_path_buf();
    let mixed_duplicate = uuid::Uuid::new_v4();
    let valid_duplicate = uuid::Uuid::new_v4();
    let stopped = uuid::Uuid::new_v4();
    for (name, id, invalid) in [
        ("mixed-valid", mixed_duplicate, false),
        ("mixed-invalid", mixed_duplicate, true),
        ("valid-first", valid_duplicate, false),
        ("valid-second", valid_duplicate, false),
        ("stopped", stopped, false),
    ] {
        let mut raw = startup_fixture_config(id);
        if invalid {
            raw = raw.replace("no_tun = true", "no_tun = 'invalid'");
        }
        std::fs::write(directory.join(format!("{name}.toml")), raw).unwrap();
    }
    let state = InstanceStateStore::new(Some(&directory));
    for id in [mixed_duplicate, valid_duplicate, stopped] {
        state.set_enabled(id, false).unwrap();
    }
    let mut startup = prepare_startup_configs(&[], Some(&directory), true)
        .await
        .unwrap();
    assert_eq!(startup.directory.config_dir_file_count, 5);
    assert_eq!(startup.files.len(), 1);
    assert_eq!(startup.files[0].config.get_id(), stopped);
    let manager = native_cli_instance_manager(false, 0)
        .unwrap()
        .with_config_path(Some(directory));
    start_prepared_configs(
        &mut startup,
        &manager,
        &state,
        &NetworkOptions::default(),
        false,
    )
    .unwrap();
    assert!(manager.instance_ids().is_empty());
    assert_eq!(manager.managed_config_ids(), vec![stopped]);
    startup.reconcile_state(&state);
    for id in [mixed_duplicate, valid_duplicate, stopped] {
        assert!(!state.is_enabled(&id));
    }
}

#[tokio::test]
async fn config_dir_startup_keeps_valid_readonly_env_and_legacy_configs() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path().to_path_buf();
    let readonly_id = uuid::Uuid::new_v4();
    let env_id = uuid::Uuid::new_v4();
    let readonly_path = directory.join(format!("{readonly_id}.toml"));
    let legacy_path = directory.join("legacy.toml");
    std::fs::write(&readonly_path, startup_fixture_config(readonly_id)).unwrap();
    std::fs::write(
        directory.join(format!("{env_id}.toml")),
        startup_fixture_config(env_id).replace(
            "no_tun = true",
            &format!("no_tun = ${{ET_STARTUP_TEST_{}:-true}}", env_id.simple()),
        ),
    )
    .unwrap();
    let legacy_raw = startup_fixture_config(uuid::Uuid::new_v4())
        .lines()
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&legacy_path, &legacy_raw).unwrap();
    let original_permissions = [readonly_path.clone(), legacy_path.clone()].map(|path| {
        (
            path.clone(),
            std::fs::metadata(&path).unwrap().permissions(),
        )
    });
    for (path, original) in &original_permissions {
        let mut permissions = original.clone();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).unwrap();
    }
    let prepared = prepare_startup_configs(&[], Some(&directory), false).await;
    for (path, original) in original_permissions {
        std::fs::set_permissions(path, original).unwrap();
    }
    let mut startup = prepared.unwrap();
    assert_eq!(startup.files.len(), 3);
    assert!(startup.files.iter().all(|file| file.control.is_read_only()));
    assert!(
        startup
            .files
            .iter()
            .all(|file| file.config.get_flags().no_tun)
    );
    let legacy = startup
        .files
        .iter()
        .find(|file| file.path == legacy_path)
        .unwrap();
    assert!(config_document_identity(legacy.config.dump().as_bytes()).is_none());
    assert!(startup.directory.unidentified_config_dir_entry);
    let manager = native_cli_instance_manager(false, 0)
        .unwrap()
        .with_config_path(Some(directory));
    start_prepared_configs(
        &mut startup,
        &manager,
        &InstanceStateStore::in_memory(),
        &NetworkOptions::default(),
        false,
    )
    .unwrap();
    assert_eq!(manager.instance_ids().len(), 3);
    assert!(manager.instance(readonly_id).is_some());
    assert!(manager.instance(env_id).is_some());
    assert_eq!(std::fs::read_to_string(legacy_path).unwrap(), legacy_raw);
    manager
        .delete_network_instances(manager.instance_ids())
        .await
        .unwrap();
}

#[tokio::test]
async fn config_dir_startup_isolates_register_run_errors_and_keeps_explicit_fatal() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path().to_path_buf();
    let explicit_invalid = directory.join("explicit-invalid.toml");
    std::fs::write(&explicit_invalid, [0xff, 0xfe]).unwrap();
    assert!(
        prepare_startup_configs(
            std::slice::from_ref(&explicit_invalid),
            Some(&directory),
            true
        )
        .await
        .is_err()
    );
    std::fs::remove_file(explicit_invalid).unwrap();
    let register_conflict = uuid::Uuid::new_v4();
    let run_conflict = uuid::Uuid::new_v4();
    let valid = uuid::Uuid::new_v4();
    for id in [register_conflict, run_conflict, valid] {
        std::fs::write(
            directory.join(format!("{id}.toml")),
            startup_fixture_config(id),
        )
        .unwrap();
    }
    let manager = native_cli_instance_manager(false, 0)
        .unwrap()
        .with_config_path(Some(directory.clone()));
    manager
        .register_managed_config(
            register_conflict,
            ConfigFileControl::new(Some(directory.join("old.toml")), Default::default()),
        )
        .unwrap();
    // A pre-existing explicit/runtime instance has no directory control.
    // Registering the directory candidate succeeds, but creation fails.
    manager
        .create(
            TomlConfigLoader::new_from_str(&startup_fixture_config(run_conflict)).unwrap(),
            (),
        )
        .unwrap();
    let mut startup = prepare_startup_configs(&[], Some(&directory), true)
        .await
        .unwrap();
    start_prepared_configs(
        &mut startup,
        &manager,
        &InstanceStateStore::in_memory(),
        &NetworkOptions::default(),
        false,
    )
    .unwrap();
    assert!(manager.instance(register_conflict).is_none());
    assert!(manager.instance(run_conflict).is_some());
    assert!(manager.instance(valid).is_some());
    manager
        .delete_network_instances(manager.instance_ids())
        .await
        .unwrap();
}

#[test]
fn config_dir_is_enabled_by_default_and_can_be_overridden() {
    let cli = parse_test_cli(["easytier-core"]).unwrap();
    let expected = std::env::var_os("ET_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.d"));
    assert_eq!(cli.config_dir, Some(expected));
    let cli = parse_test_cli(["easytier-core", "--config-dir", "custom-configs"]).unwrap();
    assert_eq!(cli.config_dir, Some(PathBuf::from("custom-configs")));
}

#[test]
fn config_server_accepts_single_comma_separated_and_repeated_values() {
    let single = parse_test_cli(["easytier-core", "-w", "udp://127.0.0.1:22020/single"]).unwrap();
    assert_eq!(
        single.config_server.unwrap(),
        vec!["udp://127.0.0.1:22020/single"]
    );

    let comma_separated = parse_test_cli([
        "easytier-core",
        "-w",
        "udp://127.0.0.1:22020/first,udp://127.0.0.1:22021/second",
    ])
    .unwrap();
    assert_eq!(
        comma_separated.config_server.unwrap(),
        vec![
            "udp://127.0.0.1:22020/first",
            "udp://127.0.0.1:22021/second"
        ]
    );

    let repeated = parse_test_cli([
        "easytier-core",
        "-w",
        "udp://127.0.0.1:22020/first",
        "-w",
        "udp://127.0.0.1:22021/second",
    ])
    .unwrap();
    assert_eq!(
        repeated.config_server.unwrap(),
        vec![
            "udp://127.0.0.1:22020/first",
            "udp://127.0.0.1:22021/second"
        ]
    );
}

#[test]
fn cli_preserves_native_state_dir_sni_and_transport_flag_presence() {
    let cli = parse_test_cli([
        "easytier-core",
        "--state-dir",
        "machine-state",
        "--sni",
        "front.example.com",
        "--p2p-disguise-mode",
        "only",
        "--close-redundant-conns-when-disguised",
        "--multi-thread",
        "false",
        "--multi-thread-count",
        "7",
    ])
    .unwrap();
    assert_eq!(cli.state_dir, Some(PathBuf::from("machine-state")));
    assert_eq!(
        cli.network_options.sni.as_deref(),
        Some("front.example.com")
    );
    let patch = &cli.network_options.flags;
    assert_eq!(patch.p2p_disguise_mode.as_deref(), Some("only"));
    assert_eq!(patch.close_redundant_conns_when_disguised, Some(true));
    assert_eq!(patch.multi_thread, Some(false));
    assert_eq!(patch.multi_thread_count, Some(7));
    assert_eq!(patch.disable_p2p, None);

    let cfg = TomlConfigLoader::new_from_str(
        r#"[flags]
p2p_disguise_mode = "disabled"
disable_p2p = true
"#,
    )
    .unwrap();
    cli.network_options.merge_into(&cfg).unwrap();
    assert_eq!(cfg.get_sni(), "front.example.com");
    // The file says `disabled`; the CLI mode wins.
    let merged = cfg.get_flags();
    assert_eq!(
        merged.p2p_disguise(),
        easytier_proto::common::P2pDisguiseMode::Only
    );
    assert!(merged.close_redundant_conns_when_disguised);
    assert!(merged.disable_p2p);
    assert!(!merged.multi_thread);
    assert_eq!(merged.multi_thread_count, 7);
}

#[test]
fn parallel_transport_cli_validates_and_can_reenable_preserved_count() {
    let cli = Cli::try_parse_from([
        "easytier-core",
        "--parallel-connections",
        "5",
        "--disable-parallel-connections",
        "--traffic-disguise",
        "video",
    ])
    .unwrap();
    let cfg = TomlConfigLoader::default();
    cli.network_options.merge_into(&cfg).unwrap();
    assert_eq!(cfg.get_parallel_connections(), 5);
    assert!(cfg.get_disable_parallel_connections());
    assert_eq!(cfg.get_traffic_disguise(), "video");
    let cli =
        Cli::try_parse_from(["easytier-core", "--disable-parallel-connections=false"]).unwrap();
    cli.network_options.merge_into(&cfg).unwrap();
    assert_eq!(cfg.get_parallel_connections(), 5);
    assert!(!cfg.get_disable_parallel_connections());
    for args in [
        vec!["easytier-core", "--parallel-connections", "0"],
        vec!["easytier-core", "--parallel-connections", "17"],
        vec!["easytier-core", "--traffic-disguise", "invalid"],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn test_cli_disguise_policy_is_validated_before_merging() {
    for mode in ["prefer", "off", "disabled", "only"] {
        let patch = parse_flags(&["easytier", "--p2p-disguise-mode", mode]).unwrap();
        assert_eq!(patch.p2p_disguise_mode.as_deref(), Some(mode));
    }
    assert!(parse_flags(&["easytier", "--p2p-disguise-mode", "disable"]).is_err());
}
