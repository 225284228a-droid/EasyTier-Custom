//! Custom tests for the management API input, kept out of the upstream
//! `tests` module so upstream test changes merge without touching them.

use super::*;

fn port_forward(
    bind_ip: &str,
    bind_port: u32,
    dst_ip: &str,
    dst_port: u32,
) -> manage::PortForwardConfig {
    manage::PortForwardConfig {
        bind_ip: bind_ip.to_owned(),
        bind_port,
        dst_ip: dst_ip.to_owned(),
        dst_port,
        proto: "tcp".to_owned(),
    }
}

#[test]
fn port_forwards_parse_ip_and_port_separately_and_survive_ipv6() {
    let mut config = standalone_config();
    config.port_forwards = vec![
        port_forward("10.0.0.1", 8080, "192.168.1.2", 80),
        port_forward("fe80::1", 8080, "2001:db8::1", 443),
    ];
    let forwards = config.gen_config().unwrap().get_port_forwards();
    assert_eq!(forwards.len(), 2);
    assert_eq!(forwards[0].bind_addr.to_string(), "10.0.0.1:8080");
    assert_eq!(forwards[1].bind_addr.to_string(), "[fe80::1]:8080");
    assert_eq!(forwards[1].dst_addr.to_string(), "[2001:db8::1]:443");
    assert_eq!(forwards[1].proto, "tcp");
}

#[test]
fn blank_port_forward_rows_are_skipped_but_invalid_rules_error() {
    let mut config = standalone_config();
    config.port_forwards = vec![
        manage::PortForwardConfig::default(),
        port_forward("10.0.0.1", 8080, "192.168.1.2", 80),
    ];
    assert_eq!(config.gen_config().unwrap().get_port_forwards().len(), 1);

    let mut config = standalone_config();
    config.port_forwards = vec![port_forward("999.0.0.1", 8080, "192.168.1.2", 80)];
    let error = config.gen_config().unwrap_err().to_string();
    assert!(error.contains("not a valid IP address"), "{error}");

    let mut config = standalone_config();
    config.port_forwards = vec![port_forward("10.0.0.1", 70000, "192.168.1.2", 80)];
    let error = config.gen_config().unwrap_err().to_string();
    assert!(error.contains("between 0 and 65535"), "{error}");

    // Port 0 binds an OS-assigned port and must stay legal.
    let mut config = standalone_config();
    config.port_forwards = vec![port_forward("127.0.0.1", 0, "10.88.0.8", 80)];
    let forwards = config.gen_config().unwrap().get_port_forwards();
    assert_eq!(forwards.len(), 1);
    assert_eq!(forwards[0].bind_addr.to_string(), "127.0.0.1:0");

    let mut config = standalone_config();
    let mut rule = port_forward("10.0.0.1", 8080, "192.168.1.2", 80);
    rule.proto = "sctp".to_owned();
    config.port_forwards = vec![rule];
    assert!(config.gen_config().is_err());
}

#[test]
fn gen_config_rejects_unknown_disguise_modes() {
    let mut config = standalone_config();
    config.p2p_disguise_mode = Some("disable".into());
    let error = config.gen_config().unwrap_err().to_string();
    assert!(error.contains("p2p_disguise_mode"), "{error}");
}

#[test]
fn port_forward_read_back_round_trip_preserves_ipv6_rules() {
    let mut config = standalone_config();
    config.port_forwards = vec![port_forward("fe80::1", 8080, "2001:db8::1", 443)];
    let forwards = config.gen_config().unwrap().get_port_forwards();

    // The management read-back path serializes the address and port as
    // separate fields, so an IPv6 rule must survive a save/load cycle.
    let round_tripped = NetworkConfig {
        port_forwards: forwards
            .iter()
            .map(|f| manage::PortForwardConfig {
                proto: f.proto.clone(),
                bind_ip: f.bind_addr.ip().to_string(),
                bind_port: f.bind_addr.port() as u32,
                dst_ip: f.dst_addr.ip().to_string(),
                dst_port: f.dst_addr.port() as u32,
            })
            .collect(),
        ..standalone_config()
    };
    let forwards = round_tripped.gen_config().unwrap().get_port_forwards();
    assert_eq!(forwards.len(), 1);
    assert_eq!(forwards[0].bind_addr.to_string(), "[fe80::1]:8080");
    assert_eq!(forwards[0].dst_addr.to_string(), "[2001:db8::1]:443");
}

#[cfg(feature = "config-write")]
#[test]
fn web_form_omits_defaults_without_changing_effective_flags() {
    let mut input = standalone_config();
    set_network_flags(&mut input, Flags::defaults().downgrade());
    input.vpn = Some(manage::VpnConfig { rules: vec![] });
    input.hostname = Some(String::new());
    input.secure_mode = Some(Default::default());

    let generated = input.gen_config().unwrap();
    let document: toml::Table = toml::from_str(&generated.dump()).unwrap();
    for field in ["flags", "vpn", "hostname", "secure_mode", "dhcp"] {
        assert!(
            !document.contains_key(field),
            "default {field} should be omitted"
        );
    }
    let restored = TomlConfigLoader::new_from_str(&generated.dump()).unwrap();
    assert_eq!(restored.get_flags(), Flags::defaults());
    assert!(restored.get_flags().private_mode);
    assert!(!restored.get_flags().enable_exit_node);

    input.enable_private_mode = Some(false);
    input.disable_encryption = Some(true);
    input.socket_mark = Some(0);
    input.instance_recv_bps_limit = Some(0);
    input.enable_exit_node = Some(true);
    let generated = input.gen_config().unwrap();
    let document: toml::Table = toml::from_str(&generated.dump()).unwrap();
    assert_eq!(document["flags"]["private_mode"].as_bool(), Some(false));
    assert_eq!(
        document["flags"]["enable_encryption"].as_bool(),
        Some(false)
    );
    assert_eq!(document["flags"]["socket_mark"].as_integer(), Some(0));
    assert_eq!(document["flags"]["enable_exit_node"].as_bool(), Some(true));
    assert!(!document.contains_key("vpn"));
    assert!(generated.get_flags().enable_exit_node);
}

#[cfg(feature = "config-write")]
#[test]
fn p2p_mode_is_the_single_saved_policy_parameter() {
    for mode in ["prefer", "off", "disabled", "only"] {
        let input = NetworkConfig {
            p2p_disguise_mode: Some(mode.to_owned()),
            ..standalone_config()
        };
        let generated = input.gen_config().unwrap();
        let document: toml::Table = toml::from_str(&generated.dump()).unwrap();
        let flags = document.get("flags").and_then(toml::Value::as_table);
        if mode == "prefer" {
            assert!(flags.is_none());
        } else {
            assert_eq!(flags.unwrap().len(), 1);
            assert_eq!(flags.unwrap()["p2p_disguise_mode"].as_str(), Some(mode));
        }
        let restored = TomlConfigLoader::new_from_str(&generated.dump()).unwrap();
        assert_eq!(restored.get_flags().p2p_disguise().as_str(), mode);
        let output = NetworkConfig::new_from_config(&restored).unwrap();
        assert_eq!(
            output.p2p_disguise_mode.as_deref(),
            (mode != "prefer").then_some(mode)
        );
    }
}

#[cfg(feature = "config-write")]
#[test]
fn p2p_defaults_and_explicit_settings_round_trip_through_toml() {
    let defaults = TomlConfigLoader::default().get_flags();
    assert_eq!(
        defaults.p2p_disguise(),
        crate::config::P2pDisguiseMode::Prefer
    );
    assert_eq!(defaults.default_protocol, "udp");
    assert!(defaults.close_redundant_conns_when_disguised);

    for (mode, protocol, cleanup) in [
        (None, None, None),
        (None, Some(""), None),
        (None, Some("  "), None),
        (Some("off"), Some("udp"), Some(false)),
        (Some("prefer"), Some("tcp"), Some(false)),
        (Some("off"), Some("tcp"), Some(true)),
    ] {
        let input = NetworkConfig {
            p2p_disguise_mode: mode.map(str::to_owned),
            p2p_prefer_protocol: protocol.map(str::to_owned),
            close_redundant_conns_when_disguised: cleanup,
            ..standalone_config()
        };
        let expected_mode = mode.unwrap_or("prefer");
        let expected_protocol = protocol
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("udp");
        let expected_cleanup = cleanup.unwrap_or(true);
        let config = input.gen_config().unwrap();
        let flags = config.get_flags();
        assert_eq!(flags.p2p_disguise().as_str(), expected_mode);
        assert_eq!(flags.default_protocol, expected_protocol);
        assert_eq!(flags.close_redundant_conns_when_disguised, expected_cleanup);

        let restored = TomlConfigLoader::new_from_str(&config.dump()).unwrap();
        let flags = restored.get_flags();
        assert_eq!(flags.p2p_disguise().as_str(), expected_mode);
        assert_eq!(flags.default_protocol, expected_protocol);
        assert_eq!(flags.close_redundant_conns_when_disguised, expected_cleanup);
        let output = NetworkConfig::new_from_config(&restored).unwrap();
        assert_eq!(
            output.p2p_disguise_mode.as_deref(),
            (expected_mode != "prefer").then_some(expected_mode)
        );
        assert_eq!(
            output.close_redundant_conns_when_disguised,
            cleanup.filter(|value| !value)
        );
        assert_eq!(
            output.p2p_prefer_protocol.as_deref(),
            (expected_protocol != "udp").then_some(expected_protocol)
        );
        #[cfg(feature = "web-client")]
        {
            let output = crate::config::api::network_config_from_toml(&restored);
            assert_eq!(
                output.p2p_disguise_mode.as_deref(),
                (expected_mode != "prefer").then_some(expected_mode)
            );
            assert_eq!(
                output.close_redundant_conns_when_disguised,
                cleanup.filter(|value| !value)
            );
            assert_eq!(
                output.p2p_prefer_protocol.as_deref(),
                (expected_protocol != "udp").then_some(expected_protocol)
            );
        }
    }
}

#[cfg(feature = "config-write")]
#[test]
fn disguise_api_flags_preserve_missing_and_explicit_values() {
    for value in [None, Some(false), Some(true)] {
        let input = NetworkConfig {
            close_redundant_conns_when_disguised: value,
            dev_name: Some(String::new()),
            encryption_algorithm: Some(String::new()),
            instance_recv_bps_limit: Some(0),
            socket_mark: Some(0),
            ..standalone_config()
        };
        let config = input.gen_config().unwrap();
        let restored = TomlConfigLoader::new_from_str(&config.dump()).unwrap();
        let output = NetworkConfig::new_from_config(&restored).unwrap();
        assert_eq!(
            output.close_redundant_conns_when_disguised,
            value.filter(|value| !value)
        );
        assert_eq!(output.dev_name, None);
        assert_eq!(output.encryption_algorithm.as_deref(), Some(""));
        assert_eq!(output.instance_recv_bps_limit, Some(0));
        assert_eq!(output.socket_mark, Some(0));
        assert_eq!(output.p2p_prefer_protocol, None);
    }
}

#[test]
fn magic_dns_zone_round_trips_missing_default_and_custom_values() {
    for zone in [None, Some("et.net."), Some("nas.internal.")] {
        let input = NetworkConfig {
            tld_dns_zone: zone.map(str::to_owned),
            ..standalone_config()
        };
        let config = input.gen_config().unwrap();
        let config = TomlConfigLoader::new_from_str(&config.dump()).unwrap();
        assert_eq!(config.get_flags().tld_dns_zone, zone.unwrap_or("et.net."));
        let output = NetworkConfig::new_from_config(&config).unwrap();
        // An explicit default is not kept, like every other flag.
        assert_eq!(
            output.tld_dns_zone.as_deref(),
            zone.filter(|zone| *zone != "et.net.")
        );
    }
}

#[test]
fn global_sni_round_trips_through_toml_model() {
    let input = NetworkConfig {
        sni: Some("www.cloudflare.com".to_owned()),
        ..standalone_config()
    };

    let config = input.gen_config().unwrap();
    assert_eq!(config.get_sni(), "www.cloudflare.com");

    let output = NetworkConfig::new_from_config(&config).unwrap();
    assert_eq!(output.sni.as_deref(), Some("www.cloudflare.com"));
}

#[test]
fn parallel_transport_settings_round_trip_and_reset_to_implicit_defaults() {
    let mut input = NetworkConfig {
        parallel_connections: Some(5),
        disable_parallel_connections: Some(true),
        traffic_disguise: Some("video".into()),
        ..standalone_config()
    };
    let config = input.gen_config().unwrap();
    let output = NetworkConfig::new_from_config(&config).unwrap();
    assert_eq!(output.parallel_connections, Some(5));
    assert_eq!(output.disable_parallel_connections, Some(true));
    assert_eq!(output.traffic_disguise.as_deref(), Some("video"));
    input.parallel_connections = Some(1);
    input.disable_parallel_connections = Some(false);
    input.traffic_disguise = Some("speedtest".into());
    let config = input.gen_config().unwrap();
    let output = NetworkConfig::new_from_config(&config).unwrap();
    assert_eq!(output.parallel_connections, None);
    assert_eq!(output.disable_parallel_connections, None);
    assert_eq!(output.traffic_disguise, None);
    for key in [
        "parallel_connections",
        "disable_parallel_connections",
        "traffic_disguise",
    ] {
        assert!(!config.dump().contains(key));
    }
    input.parallel_connections = Some(17);
    assert!(input.gen_config().is_err());
    input.parallel_connections = Some(1);
    input.traffic_disguise = Some("invalid".into());
    assert!(input.gen_config().is_err());
}

#[cfg(feature = "browser-config")]
#[test]
fn browser_merge_updates_and_clears_magic_dns_zone() {
    let original = "[flags]\ntld_dns_zone = \"old.internal.\"\n";
    for zone in [None, Some("et.net."), Some("nas.internal.")] {
        let input = NetworkConfig {
            tld_dns_zone: zone.map(str::to_owned),
            ..standalone_config()
        };
        let merged = merge_network_config_toml(original, &input).unwrap();
        let output = TomlConfigLoader::new_from_str(&merged).unwrap();
        assert_eq!(output.get_flags().tld_dns_zone, zone.unwrap_or("et.net."));
        assert_eq!(
            output.get_flags_patch().tld_dns_zone.as_deref(),
            zone.filter(|zone| *zone != "et.net.")
        );
    }
}

#[test]
fn display_name_names_the_instance_and_round_trips() {
    let mut config = NetworkConfig {
        network_name: Some("office".into()),
        network_secret: Some("secret".into()),
        networking_method: Some(manage::NetworkingMethod::Standalone as i32),
        ..Default::default()
    };
    let toml = config.gen_config().unwrap();
    assert_eq!(toml.get_inst_name(), "office");
    assert_eq!(
        NetworkConfig::new_from_config(&toml).unwrap().instance_name,
        None
    );
    config.instance_name = Some("Head office".into());
    let toml = config.gen_config().unwrap();
    assert_eq!(toml.get_inst_name(), "Head office");
    assert_eq!(
        NetworkConfig::new_from_config(&toml)
            .unwrap()
            .instance_name
            .as_deref(),
        Some("Head office")
    );
    // A console-created file starts named by its instance id.
    toml.set_inst_name(toml.get_id().to_string());
    assert_eq!(
        NetworkConfig::new_from_config(&toml).unwrap().instance_name,
        None
    );
}

#[cfg(feature = "browser-config")]
#[test]
fn browser_merge_updates_sni_protocol_and_disguise_flags() {
    let original = r#"
sni = "old.example.com"

[flags]
default_protocol = "tcp"
only_use_wss_http3_for_hole_punching = true
prefer_wss_http3_for_p2p = true
disable_wss_http3_for_p2p = false
close_redundant_conns_when_disguised = true
multi_thread_count = 7
"#;
    let parsed = TomlConfigLoader::new_from_str(original).unwrap();
    let mut config = NetworkConfig::new_from_config(&parsed).unwrap();
    // The legacy booleans load as the equivalent mode.
    assert_eq!(config.p2p_disguise_mode.as_deref(), Some("only"));
    config.sni = Some("new.example.com".to_owned());
    config.p2p_prefer_protocol = Some("udp".to_owned());
    config.p2p_disguise_mode = Some("off".to_owned());
    config.close_redundant_conns_when_disguised = Some(false);

    let merged = merge_network_config_toml(original, &config).unwrap();
    let restored = TomlConfigLoader::new_from_str(&merged).unwrap();
    let output = NetworkConfig::new_from_config(&restored).unwrap();
    assert_eq!(output.sni.as_deref(), Some("new.example.com"));
    assert_eq!(output.p2p_prefer_protocol, None);
    assert_eq!(output.p2p_disguise_mode.as_deref(), Some("off"));
    assert!(!merged.contains("wss_http3"), "{merged}");
    assert_eq!(output.close_redundant_conns_when_disguised, Some(false));
    assert_eq!(restored.get_flags().multi_thread_count, 7);
}

#[test]
fn upstream_dhcp_and_custom_transport_policy_survive_toml_round_trip() {
    let input = NetworkConfig {
        dhcp: Some(true),
        dhcp_ipv4_cidr: Some("10.55.0.0/24".to_owned()),
        dhcpv6: Some(true),
        dhcp_ipv6_cidr: Some("fd55::/64".to_owned()),
        sni: Some("overlay.example.com".to_owned()),
        p2p_disguise_mode: Some("only".to_owned()),
        ..standalone_config()
    };
    let generated = input.gen_config().unwrap();
    let loaded = TomlConfigLoader::new_from_str(&generated.dump()).unwrap();
    let readback = NetworkConfig::new_from_config(&loaded).unwrap();
    assert_eq!(readback.dhcp_ipv4_cidr, input.dhcp_ipv4_cidr);
    assert_eq!(readback.dhcpv6, input.dhcpv6);
    assert_eq!(readback.dhcp_ipv6_cidr, input.dhcp_ipv6_cidr);
    assert_eq!(readback.sni, input.sni);
    assert_eq!(readback.p2p_disguise_mode, input.p2p_disguise_mode);
}
