//! Custom tests for the TOML configuration loader, kept out of the upstream
//! test modules so upstream test changes merge without touching them.

use super::*;

/// Configuration files are parsed strictly, so every sample we ship must
/// only use fields this build understands.
#[test]
fn shipped_sample_configs_load() {
    let install = include_str!("../../../../../script/install.sh");
    let start = "cat >$INSTALL_PATH/config/default.conf <<EOF\n";
    let body = &install[install.find(start).unwrap() + start.len()..];
    let install_default = &body[..body.find("\nEOF").unwrap()];
    for (name, sample) in [
        ("script/install.sh default.conf", install_default),
        (
            "easytier-magisk/config/config.toml",
            include_str!("../../../../../easytier-contrib/easytier-magisk/config/config.toml"),
        ),
    ] {
        if let Err(error) = TomlConfig::new_from_str_with_source(name, sample) {
            panic!("{name}: {error}");
        }
    }
}

#[cfg(feature = "config-write")]
#[test]
fn network_sync_stamp_round_trips_and_is_omitted_when_unset() {
    let config = TomlConfig::new_from_str("").unwrap();
    assert_eq!(config.get_network_sync_at(), None);
    assert!(!config.dump().contains("network_sync_at"));
    assert!(!config.dump().contains("advanced_sync_at"));
    config.set_network_sync_at(Some(1_793_587_936_027));
    config.set_advanced_sync_at(Some(1_793_587_936_028));
    let restored = TomlConfig::new_from_str(&config.dump()).unwrap();
    assert_eq!(restored.get_network_sync_at(), Some(1_793_587_936_027));
    assert_eq!(restored.get_advanced_sync_at(), Some(1_793_587_936_028));
    #[cfg(feature = "web-client")]
    assert_eq!(
        crate::config::api::network_config_from_toml(&restored).network_sync_at,
        Some(1_793_587_936_027)
    );
}

#[test]
fn parallel_tuning_validates_and_omits_defaults() {
    let config = TomlConfig::new_from_str("parallel_connections = 1\ndisable_parallel_connections = false\ntraffic_disguise = 'speedtest'\n").unwrap();
    assert_eq!(config.get_parallel_connections(), 1);
    assert!(!config.get_disable_parallel_connections());
    assert_eq!(config.get_traffic_disguise(), "speedtest");
    for key in [
        "parallel_connections",
        "disable_parallel_connections",
        "traffic_disguise",
    ] {
        assert!(!config.dump().contains(key));
    }
    let config = TomlConfig::new_from_str("parallel_connections = 16\ndisable_parallel_connections = true\ntraffic_disguise = 'video'\n").unwrap();
    let restored = TomlConfig::new_from_str(&config.dump()).unwrap();
    assert_eq!(restored.get_parallel_connections(), 16);
    assert!(restored.get_disable_parallel_connections());
    assert_eq!(restored.get_traffic_disguise(), "video");
    restored.set_disable_parallel_connections(Some(false));
    assert_eq!(restored.get_parallel_connections(), 16);
    assert!(!restored.dump().contains("disable_parallel_connections"));
    for input in [
        "parallel_connections = 0",
        "parallel_connections = 17",
        "parallel_connections = 1.5",
        "traffic_disguise = 'invalid'",
    ] {
        assert!(TomlConfig::new_from_str(input).is_err(), "{input}");
    }
}

#[test]
fn omitted_or_empty_protocol_uses_udp_and_enables_cleanup() {
    for input in [
        "",
        "[flags]",
        "[flags]\ndefault_protocol = \"\"",
        "[flags]\ndefault_protocol = \"  \"",
    ] {
        let config = TomlConfigLoader::new_from_str(input).unwrap();
        assert_eq!(config.get_flags().default_protocol, "udp", "{input}");
        assert!(config.get_flags().close_redundant_conns_when_disguised);
    }
}

#[cfg(feature = "config-write")]
#[test]
fn explicit_protocol_and_cleanup_survive_config_dump_and_reload() {
    for (protocol, cleanup) in [("udp", true), ("tcp", false)] {
        let config = TomlConfigLoader::new_from_str(&format!(
            "[flags]\ndefault_protocol = \"{protocol}\"\nclose_redundant_conns_when_disguised = {cleanup}\n"
        ))
        .unwrap();
        assert_eq!(config.get_flags().default_protocol, protocol);
        assert_eq!(
            config.get_flags().close_redundant_conns_when_disguised,
            cleanup
        );
        let dumped = config.dump();
        if protocol == "tcp" {
            assert!(dumped.contains("default_protocol = \"tcp\""));
            assert!(dumped.contains("close_redundant_conns_when_disguised = false"));
        }
        let reloaded = TomlConfigLoader::new_from_str(&dumped).unwrap();
        assert_eq!(reloaded.get_flags().default_protocol, protocol);
        assert_eq!(
            reloaded.get_flags().close_redundant_conns_when_disguised,
            cleanup
        );
    }
}

#[cfg(feature = "config-write")]
#[test]
fn form_default_pruning_preserves_both_dhcp_subnets() {
    let config = TomlConfig::new_from_str("dhcp = '10.20.0.0/24'\ndhcpv6 = 'fd12::/64'").unwrap();
    config.prune_form_defaults();
    let restored = TomlConfig::new_from_str(&config.dump()).unwrap();
    assert_eq!(restored.get_dhcp_config(), config.get_dhcp_config());
    assert_eq!(restored.get_dhcpv6_config(), config.get_dhcpv6_config());
    config.set_dhcp(false);
    config.set_dhcpv6(false);
    config.prune_form_defaults();
    assert!(!config.dump().contains("dhcp"));
}

#[test]
fn legacy_disguise_flags_load_as_the_equivalent_mode() {
    for (input, mode) in [
        ("prefer_wss_http3_for_p2p = true", "prefer"),
        ("prefer_wss_http3_for_p2p = false", "off"),
        ("disable_wss_http3_for_p2p = true", "disabled"),
        ("only_use_wss_http3_for_hole_punching = true", "only"),
        (
            "prefer_wss_http3_for_p2p = true\nonly_use_wss_http3_for_hole_punching = true",
            "only",
        ),
        // A mode that is already set wins over the booleans it replaced.
        (
            "p2p_disguise_mode = 'off'\ndisable_wss_http3_for_p2p = true",
            "off",
        ),
    ] {
        let config = TomlConfig::new_from_str(&format!("[flags]\n{input}\nmtu = 1300\n"))
            .unwrap_or_else(|error| panic!("{input}: {error}"));
        assert_eq!(config.get_flags().p2p_disguise().as_str(), mode, "{input}");
        assert_eq!(config.get_flags().mtu, 1300);
        let dumped = config.dump();
        assert!(!dumped.contains("wss_http3"), "{dumped}");
    }
    let error = TomlConfig::new_from_str(
        "[flags]\nonly_use_wss_http3_for_hole_punching = true\ndisable_wss_http3_for_p2p = true\n",
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("mutually exclusive"),
        "{error:#}"
    );
    assert!(TomlConfig::new_from_str("[flags]\nprefer_wss_http3_for_p2p = 'yes'\n").is_err());
}

#[test]
fn mode_patches_keep_unrelated_flags() {
    let config = TomlConfig::new_from_str("[flags]\np2p_disguise_mode = 'off'\n").unwrap();
    config.patch_flags(FlagsPatch {
        latency_first: Some(true),
        ..Default::default()
    });
    assert_eq!(
        config.get_flags_patch().p2p_disguise_mode.as_deref(),
        Some("off")
    );
    assert!(config.get_flags().latency_first);
    config.patch_flags(FlagsPatch {
        p2p_disguise_mode: Some("only".into()),
        ..Default::default()
    });
    assert_eq!(config.get_flags().p2p_disguise().as_str(), "only");
    assert!(config.get_flags().latency_first);
}

#[cfg(feature = "config-write")]
#[test]
fn flags_patch_preserves_explicit_false_zero_and_empty_values() {
    let config = TomlConfig::new_from_str(
        r#"[flags]
default_protocol = "tcp"
dev_name = "old-device"
encryption_algorithm = "chacha20"
relay_network_whitelist = "allowed"
mtu = 1420
instance_recv_bps_limit = 1024
socket_mark = 66
only_use_wss_http3_for_hole_punching = false
prefer_wss_http3_for_p2p = true
disable_wss_http3_for_p2p = true
close_redundant_conns_when_disguised = true
"#,
    )
    .unwrap();
    config.patch_flags(FlagsPatch::default());
    assert_eq!(config.get_flags().default_protocol, "tcp");
    assert_eq!(config.get_flags().mtu, 1420);

    let update = FlagsPatch {
        default_protocol: Some(String::new()),
        dev_name: Some(String::new()),
        encryption_algorithm: Some(String::new()),
        relay_network_whitelist: Some(String::new()),
        mtu: Some(0),
        instance_recv_bps_limit: Some(0),
        socket_mark: Some(0),
        p2p_disguise_mode: Some("off".into()),
        close_redundant_conns_when_disguised: Some(false),
        ..Default::default()
    };
    config.patch_flags(update.clone());
    let restored = TomlConfig::new_from_str(&config.dump()).unwrap();
    assert_eq!(restored.get_flags_patch(), update);

    let flags = restored.get_flags();
    assert_eq!(flags.default_protocol, "udp");
    assert_eq!(flags.dev_name, "");
    assert_eq!(flags.encryption_algorithm, "");
    assert_eq!(flags.relay_network_whitelist, "");
    assert_eq!(flags.mtu, 0);
    assert_eq!(flags.instance_recv_bps_limit, 0);
    assert_eq!(flags.socket_mark, Some(0));
    assert!(!flags.close_redundant_conns_when_disguised);
}

#[test]
fn blank_protocol_resolves_to_udp_without_losing_patch_presence() {
    let defaults = TomlConfig::default();
    assert_eq!(defaults.get_flags_patch().default_protocol, None);
    assert_eq!(defaults.get_flags().default_protocol, "udp");
    for protocol in ["", "  ", "\t"] {
        let config =
            TomlConfig::new_from_str(&format!("[flags]\ndefault_protocol = {protocol:?}")).unwrap();
        assert_eq!(
            config.get_flags_patch().default_protocol.as_deref(),
            Some(protocol)
        );
        assert_eq!(config.get_flags().default_protocol, "udp");
    }
}

#[cfg(feature = "config-write")]
#[test]
fn test_vpn_config_roundtrip() {
    let config = TomlConfigLoader::default();
    let mut vpn = crate::config::vpn::VpnConfig::default();
    vpn.rules.push(crate::config::vpn::VpnRuleConfig {
        cidr: "0.0.0.0/0".parse().unwrap(),
        exit: Some("10.144.144.2".parse().unwrap()),
    });

    config.set_vpn(vpn.clone());
    assert_eq!(config.get_vpn(), vpn);

    let dumped = config.dump();
    let loaded = TomlConfigLoader::new_from_str(&dumped).unwrap();
    assert_eq!(loaded.get_vpn(), vpn);
}
