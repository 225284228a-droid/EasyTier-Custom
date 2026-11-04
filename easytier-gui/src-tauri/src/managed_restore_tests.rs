use super::*;

#[test]
fn normal_mode_accepts_a_saved_service_rpc_address() {
    let (bind, connect) = normalize_normal_mode_rpc_portal("127.0.0.1:15999").unwrap();
    assert_eq!(bind.scheme(), "tcp");
    assert_eq!(bind.host_str(), Some("127.0.0.1"));
    assert_eq!(bind.port(), Some(15999));
    assert_eq!(connect, bind);
}

#[test]
fn normal_mode_normalizes_wildcard_connect_addrs() {
    let (bind, connect) = normalize_normal_mode_rpc_portal("0.0.0.0:15999").unwrap();
    assert_eq!(bind.host_str(), Some("0.0.0.0"));
    assert_eq!(connect.host_str(), Some("127.0.0.1"));

    let (bind, connect) = normalize_normal_mode_rpc_portal("tcp://[::]:15999").unwrap();
    assert_eq!(bind.host_str(), Some("[::]"));
    assert_eq!(connect.host_str(), Some("[::1]"));

    // The GUI never connects out via a wildcard addr; the connect URL
    // must be dialable.
    assert!(connect.socket_addrs(|| None).is_ok());
}

#[tokio::test]
async fn normal_mode_restores_the_service_config_dir_state() {
    let dir = tempfile::tempdir().unwrap();
    let enabled = TomlConfigLoader::default();
    enabled.set_inst_name("enabled".to_string());
    enabled.set_listeners(Vec::new());
    let mut enabled_flags = enabled.get_flags();
    enabled_flags.no_tun = true;
    enabled.set_flags(enabled_flags);
    let enabled_id = enabled.get_id();
    std::fs::write(
        dir.path().join(format!("{enabled_id}.toml")),
        enabled.dump(),
    )
    .unwrap();

    let disabled = TomlConfigLoader::default();
    disabled.set_inst_name("disabled".to_string());
    disabled.set_listeners(Vec::new());
    let mut disabled_flags = disabled.get_flags();
    disabled_flags.no_tun = true;
    disabled.set_flags(disabled_flags);
    let disabled_id = disabled.get_id();
    std::fs::write(dir.path().join("named-disabled.toml"), disabled.dump()).unwrap();

    let stale_id = Uuid::new_v4();
    let state_store = InstanceStateStore::new(Some(dir.path()));
    state_store.set_enabled(disabled_id, false).unwrap();
    state_store.set_enabled(stale_id, false).unwrap();
    drop(state_store);

    let state_store = InstanceStateStore::new(Some(dir.path()));
    let manager = native_instance_manager_with_config_dir(Some(dir.path().to_path_buf()));
    restore_managed_configs(&manager, &state_store, dir.path())
        .await
        .unwrap();

    assert!(manager.instance_ids().contains(&enabled_id));
    assert!(!manager.instance_ids().contains(&disabled_id));
    assert_eq!(state_store.disabled_instance_ids(), vec![disabled_id]);
    assert_eq!(
        manager.managed_config_control(disabled_id).unwrap().path,
        Some(dir.path().join("named-disabled.toml"))
    );
    manager.retain_network_instances(&[]).await.unwrap();
}

#[tokio::test]
async fn normal_mode_isolates_invalid_config_files() {
    // The GUI normal mode shares the service config directory, so a
    // broken file must not keep the whole local backend (and with it the
    // configuration editor that could repair the file) from starting.
    let dir = tempfile::tempdir().unwrap();
    let valid = TomlConfigLoader::default();
    valid.set_inst_name("valid".to_string());
    valid.set_listeners(Vec::new());
    let mut valid_flags = valid.get_flags();
    valid_flags.no_tun = true;
    valid.set_flags(valid_flags);
    let valid_id = valid.get_id();
    std::fs::write(dir.path().join(format!("{valid_id}.toml")), valid.dump()).unwrap();
    std::fs::write(dir.path().join("broken.toml"), "not = [a valid toml").unwrap();

    let state_store = InstanceStateStore::new(Some(dir.path()));
    let manager = native_instance_manager_with_config_dir(Some(dir.path().to_path_buf()));
    restore_managed_configs(&manager, &state_store, dir.path())
        .await
        .unwrap();

    assert!(manager.instance_ids().contains(&valid_id));
    manager.retain_network_instances(&[]).await.unwrap();
}
