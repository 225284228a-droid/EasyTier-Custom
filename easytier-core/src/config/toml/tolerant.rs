//! Recovery is reserved for documents loaded at startup. Strict deserialization
//! remains available for API validation and editing.

use super::{Config, normalize_secure_mode_config};
use serde_path_to_error::Segment;

pub(super) struct Warning {
    pub path: String,
    pub reason: &'static str,
}

fn warn(warnings: &mut Vec<Warning>, path: impl Into<String>, reason: &'static str) {
    warnings.push(Warning {
        path: path.into(),
        reason,
    });
}

pub(super) fn load(input: &str) -> anyhow::Result<(Config, Vec<Warning>)> {
    let mut warnings = Vec::new();
    let mut document = parse_document(input, &mut warnings)?;
    let mut config: Config = loop {
        match serde_path_to_error::deserialize(document.clone()) {
            Ok(config) => break config,
            Err(error) => {
                let mut path: Vec<_> = error.path().iter().cloned().collect();
                // A denied unknown key can be reported at its containing map.
                // Removing that key preserves the other fields in the map.
                if let Some(key) = error
                    .inner()
                    .message()
                    .strip_prefix("unknown field `")
                    .and_then(|message| message.split_once('`').map(|(key, _)| key))
                    && !matches!(path.last(), Some(Segment::Map { key: last }) if last == key)
                {
                    path.push(Segment::Map {
                        key: key.to_owned(),
                    });
                }
                let name = path_name(&path);
                if !remove(&mut document, &path) {
                    // Never silently discard the entire document or spin if a
                    // future deserializer produces an unsupported error path.
                    anyhow::bail!("cannot recover invalid configuration field {name}");
                }
                warn(
                    &mut warnings,
                    name,
                    "invalid type, value, or incomplete entry",
                );
            }
        }
    };
    sanitize(&mut config, &mut warnings);
    super::TomlConfig::normalize_config_source(&mut config);
    Ok((config, warnings))
}

fn path_name(path: &[Segment]) -> String {
    let mut result = String::new();
    for segment in path {
        match segment {
            Segment::Map { key } => {
                if !result.is_empty() {
                    result.push('.');
                }
                result.push_str(key);
            }
            Segment::Seq { index } => result.push_str(&format!("[{index}]")),
            _ => {}
        }
    }
    result
}

fn remove(value: &mut toml::Value, path: &[Segment]) -> bool {
    let Some((first, remaining)) = path.split_first() else {
        return false;
    };
    match (value, first) {
        (toml::Value::Table(table), Segment::Map { key }) => {
            if remaining.is_empty() {
                table.remove(key).is_some()
            } else {
                table
                    .get_mut(key)
                    .is_some_and(|value| remove(value, remaining))
            }
        }
        (toml::Value::Array(array), Segment::Seq { index }) if *index < array.len() => {
            if remaining.is_empty() {
                array.remove(*index);
                true
            } else {
                remove(&mut array[*index], remaining)
            }
        }
        _ => false,
    }
}

fn statement_start(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('[')
        || line.split_once('=').is_some_and(|(key, _)| {
            !key.trim().is_empty()
                && !key.trim().starts_with('#')
                && key
                    .trim()
                    .chars()
                    .all(|c| c.is_alphanumeric() || "_-.\"' ".contains(c))
        })
}

fn parse_document(input: &str, warnings: &mut Vec<Warning>) -> anyhow::Result<toml::Value> {
    let mut source = input.to_owned();
    let ranges: Vec<_> = input
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some(start..*offset)
        })
        .collect();
    loop {
        let error = match toml::from_str::<toml::Table>(&source) {
            Ok(table) => return Ok(toml::Value::Table(table)),
            Err(error) => error,
        };
        let offset = error.span().map_or(0, |span| span.start);
        let Some(mut end_line) = ranges
            .iter()
            .position(|range| range.end > offset)
            .or_else(|| ranges.len().checked_sub(1))
        else {
            anyhow::bail!("cannot locate malformed TOML statement");
        };
        // Locate the pending statement from the last complete document prefix.
        // A missing closing delimiter can otherwise point at a later valid
        // assignment or EOF and cause unrelated configuration to be erased.
        let mut start_line = (0..=end_line)
            .rev()
            .find(|line| toml::from_str::<toml::Table>(&source[..ranges[*line].start]).is_ok())
            .unwrap_or(0);
        while start_line < end_line {
            let line = source[ranges[start_line].clone()].trim();
            if !line.is_empty() && !line.starts_with('#') {
                break;
            }
            start_line += 1;
        }
        if let Some(next_statement) =
            (start_line + 1..=end_line).find(|line| statement_start(&source[ranges[*line].clone()]))
        {
            end_line = next_statement - 1;
        }
        // A malformed table header must not move its body into the previous
        // table. Skip its body through the next table header as well.
        if source[ranges[start_line].clone()]
            .trim_start()
            .starts_with('[')
        {
            while end_line + 1 < ranges.len()
                && !source[ranges[end_line + 1].clone()]
                    .trim_start()
                    .starts_with('[')
            {
                end_line += 1;
            }
        }
        let range = ranges[start_line].start..ranges[end_line].end;
        let mut bytes = source.into_bytes();
        let mut changed = false;
        for byte in &mut bytes[range] {
            if !matches!(*byte, b'\n' | b'\r' | b' ') {
                *byte = b' ';
                changed = true;
            }
        }
        source = String::from_utf8(bytes).expect("masked TOML remains UTF-8");
        warn(
            warnings,
            format!("line {}", start_line + 1),
            "invalid TOML syntax",
        );
        if !changed {
            // All retries erase at least one non-whitespace byte, providing a
            // finite bound even for malformed multiline input.
            anyhow::bail!(
                "cannot recover malformed TOML statement at line {}",
                start_line + 1
            );
        }
    }
}

fn sanitize(config: &mut Config, warnings: &mut Vec<Warning>) {
    if config
        .flags
        .encryption_algorithm
        .as_ref()
        .is_some_and(|algorithm| {
            algorithm
                .parse::<crate::config::EncryptionAlgorithm>()
                .is_err()
        })
    {
        config.flags.encryption_algorithm = None;
        warn(
            warnings,
            "flags.encryption_algorithm",
            "unknown encryption algorithm",
        );
    }
    if config.flags.data_compress_algo.is_some_and(|algorithm| {
        crate::proto::common::CompressionAlgoPb::try_from(algorithm)
            .ok()
            .and_then(|algorithm| crate::packet::CompressorAlgo::try_from(algorithm).ok())
            .is_none_or(|algorithm| !algorithm.is_available())
    }) {
        config.flags.data_compress_algo = None;
        warn(
            warnings,
            "flags.data_compress_algo",
            "unknown or unavailable compression algorithm",
        );
    }
    if config.socks5_proxy.as_ref().is_some_and(|url| {
        url.host_str().zip(url.port()).is_none_or(|(host, port)| {
            format!("{host}:{port}")
                .parse::<std::net::SocketAddr>()
                .is_err()
        })
    }) {
        config.socks5_proxy = None;
        warn(
            warnings,
            "socks5_proxy",
            "SOCKS5 bind address requires an IP address and port",
        );
    }
    let credential_node = config
        .network_identity
        .as_ref()
        .is_some_and(|identity| identity.network_secret.as_deref().is_none_or(str::is_empty))
        && config
            .secure_mode
            .as_ref()
            .is_some_and(|secure| secure.enabled);
    if credential_node && !config.managed_credentials.is_empty() {
        config.managed_credentials.clear();
        warn(
            warnings,
            "managed_credentials",
            "credential nodes cannot manage credentials",
        );
    }
    let mut accepted_credentials = Vec::with_capacity(config.managed_credentials.len());
    let mut credential_index = 0;
    config.managed_credentials.retain(|credential| {
        accepted_credentials.push(credential.clone());
        let valid = crate::peers::credential_manager::validate_managed_credential_configs(
            &accepted_credentials,
        )
        .is_ok();
        if !valid {
            accepted_credentials.pop();
            warn(
                warnings,
                format!("managed_credentials[{credential_index}]"),
                "invalid managed credential or duplicate identity",
            );
        }
        credential_index += 1;
        valid
    });
    if config
        .ipv4
        .as_ref()
        .is_some_and(|value| value.parse::<cidr::Ipv4Inet>().is_err())
    {
        config.ipv4 = None;
        warn(warnings, "ipv4", "invalid IPv4 address");
    }
    if config
        .ipv6
        .as_ref()
        .is_some_and(|value| value.parse::<cidr::Ipv6Inet>().is_err())
    {
        config.ipv6 = None;
        warn(warnings, "ipv6", "invalid IPv6 address");
    }
    if let Some(vpn) = &mut config.vpn {
        let mut cidrs = std::collections::BTreeSet::new();
        let mut index = 0;
        vpn.rules.retain(|rule| {
            let valid = !rule.exit.is_unspecified()
                && !rule.exit.is_multicast()
                && !rule.exit.is_loopback()
                && cidrs.insert(rule.cidr);
            if !valid {
                warn(
                    warnings,
                    format!("vpn.rules[{index}]"),
                    "invalid exit address or duplicate subnet",
                );
            }
            index += 1;
            valid
        });
    }
    if let Some(networks) = &mut config.proxy_network {
        for (index, network) in networks.iter_mut().enumerate() {
            if network
                .mapped_cidr
                .is_some_and(|mapped| mapped.network_length() != network.cidr.network_length())
            {
                network.mapped_cidr = None;
                warn(
                    warnings,
                    format!("proxy_network[{index}].mapped_cidr"),
                    "mapped subnet prefix differs from proxy subnet",
                );
            }
        }
    }
    if let Some(forwards) = &mut config.port_forward {
        let mut index = 0;
        forwards.retain(|forward| {
            let valid = matches!(forward.proto.to_ascii_lowercase().as_str(), "tcp" | "udp");
            if !valid {
                warn(
                    warnings,
                    format!("port_forward[{index}].proto"),
                    "unsupported forwarding protocol",
                );
            }
            index += 1;
            valid
        });
    }
    if config.flags.validate_p2p_disguise().is_err() {
        if config
            .flags
            .p2p_disguise_mode
            .as_ref()
            .is_some_and(|mode| !mode.is_empty())
        {
            config.flags.p2p_disguise_mode = None;
            warn(
                warnings,
                "flags.p2p_disguise_mode",
                "unsupported P2P disguise mode",
            );
        }
        if config.flags.validate_p2p_disguise().is_err() {
            config.flags.only_use_wss_http3_for_hole_punching = None;
            warn(
                warnings,
                "flags.only_use_wss_http3_for_hole_punching",
                "conflicts with disabling P2P disguise",
            );
        }
    }
    if let Some(secure_mode) = &mut config.secure_mode
        && secure_mode.enabled
    {
        if secure_mode.local_private_key.is_some() && secure_mode.private_key().is_err() {
            secure_mode.local_private_key = None;
            warn(
                warnings,
                "secure_mode.local_private_key",
                "invalid private key",
            );
        }
        if normalize_secure_mode_config(secure_mode.clone()).is_err() {
            secure_mode.local_public_key = None;
            warn(
                warnings,
                "secure_mode.local_public_key",
                "invalid or mismatched public key",
            );
        }
    }
}

pub(super) fn sanitize_portal(
    config: &super::TomlConfig,
    warnings: &mut Vec<Warning>,
) -> anyhow::Result<()> {
    use super::ConfigLoader as _;
    use crate::gateway::vpn_portal::{PortalClientConfig, PortalRuntimeConfig, validate_clients};
    use base64::Engine as _;

    let Some(mut portal) = config
        .get_vpn_portal_config()
        .filter(|portal| portal.enabled != Some(false))
    else {
        return Ok(());
    };
    // The native WireGuard adapter requires a dedicated 32-byte key. Disabling
    // just the portal preserves the rest of the node and its stored clients.
    let key_valid = portal.wireguard_private_key.as_deref().is_some_and(|key| {
        base64::engine::general_purpose::STANDARD
            .decode(key)
            .is_ok_and(|key| key.len() == 32)
    });
    if !key_valid {
        portal.enabled = Some(false);
        config.set_vpn_portal_config(portal);
        warn(
            warnings,
            "vpn_portal_config.wireguard_private_key",
            "portal disabled because its dedicated private key is missing or invalid",
        );
        return Ok(());
    }
    let normalized = crate::instance::CoreInstanceConfig::from_toml(config)?;
    let runtime = crate::config::runtime::CoreInstanceRuntimeConfig {
        services: normalized.connectivity.runtime,
        peer: std::sync::Arc::new(normalized.peer.snapshot),
    };
    let mut candidate = PortalRuntimeConfig::default();
    if validate_clients(&candidate, &runtime).is_err() {
        portal.enabled = Some(false);
        config.set_vpn_portal_config(portal);
        warn(
            warnings,
            "vpn_portal_config",
            "portal disabled because its node requirements are not satisfied",
        );
        return Ok(());
    }
    let mut index = 0;
    portal.clients.retain(|client| {
        candidate.clients.push(PortalClientConfig {
            name: client.name.clone(),
            virtual_ip: client.virtual_ip,
            groups: client.groups.clone(),
        });
        let valid = validate_clients(&candidate, &runtime).is_ok();
        if !valid {
            candidate.clients.pop();
            warn(
                warnings,
                format!("vpn_portal_config.clients[{index}]"),
                "invalid portal client or incompatible node configuration",
            );
        }
        index += 1;
        valid
    });
    config.set_vpn_portal_config(portal);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::toml::{ConfigLoader, TomlConfig};

    #[test]
    fn retains_valid_siblings_and_array_items() {
        let input = r#"
hostname = "survives"
dhcp = "yes"
listeners = ["tcp://0.0.0.0:11010", "invalid url", "udp://0.0.0.0:11011"]
[network_identity]
network_name = "my-network"
network_secret = 123
[flags]
mtu = "bad"
enable_ipv6 = false
[[peer]]
uri = "tcp://example.com:11010"
peer_public_key = 456
[[peer]]
uri = "not a URL"
[[peer]]
uri = "udp://example.com:11011"
[vpn]
exit_enabled = true
unknown_future_field = 1
[[vpn.rules]]
cidr = "10.0.0.0/8"
exit = "10.1.2.3"
[[vpn.rules]]
cidr = "invalid"
exit = "10.2.3.4"
"#;
        let (config, warnings) = load(input).unwrap();
        let config = TomlConfig::new_from_config(config).unwrap();
        assert_eq!(config.get_hostname(), "survives");
        assert!(!config.get_dhcp());
        assert_eq!(config.get_listeners().unwrap().len(), 2);
        assert_eq!(config.get_network_identity().network_name, "my-network");
        assert_eq!(config.get_peers().len(), 2);
        assert_eq!(config.get_peers()[0].peer_public_key, None);
        assert_eq!(config.get_flags_patch().enable_ipv6, Some(false));
        assert_eq!(config.get_flags_patch().mtu, None);
        assert!(config.get_vpn().exit_enabled);
        assert_eq!(config.get_vpn().rules.len(), 1);
        assert!(warnings.iter().any(|warning| warning.path == "dhcp"));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.path == "listeners[1]")
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.path.contains("unknown_future_field"))
        );
        assert!(TomlConfig::new_from_str(input).is_err());
    }

    #[test]
    fn semantic_errors_drop_only_unusable_values() {
        let input = r#"
hostname = "valid"
ipv4 = "invalid"
[flags]
disable_wss_http3_for_p2p = true
only_use_wss_http3_for_hole_punching = true
mtu = 1400
encryption_algorithm = "invalid-algorithm"
[secure_mode]
enabled = true
local_private_key = "secret-invalid-key-do-not-log"
local_public_key = "also-invalid"
[vpn]
exit_enabled = true
[[vpn.rules]]
cidr = "10.0.0.0/8"
exit = "127.0.0.1"
[[vpn.rules]]
cidr = "10.0.0.0/8"
exit = "10.0.0.2"
[[vpn.rules]]
cidr = "10.0.0.0/8"
exit = "10.0.0.3"
[[vpn.rules]]
cidr = "192.168.0.0/16"
exit = "10.0.0.4"
"#;
        let (config, warnings) = load(input).unwrap();
        let config = TomlConfig::new_from_config(config).unwrap();
        assert_eq!(config.get_hostname(), "valid");
        assert_eq!(config.get_ipv4(), None);
        assert_eq!(config.get_flags_patch().mtu, Some(1400));
        assert_eq!(config.get_flags_patch().encryption_algorithm, None);
        assert!(config.get_flags().disable_wss_http3_for_p2p);
        assert!(!config.get_flags().only_use_wss_http3_for_hole_punching);
        assert_eq!(config.get_vpn().rules.len(), 2);
        assert!(config.get_secure_mode().unwrap().private_key().is_ok());
        assert!(
            warnings
                .iter()
                .all(|warning| !warning.reason.contains("secret-invalid"))
        );
    }

    #[test]
    fn syntax_recovery_keeps_following_assignments_and_tables() {
        for input in [
            "hostname = 'valid'\ndhcp = nope\ninstance_name = 'kept'\n[flags]\nmtu = 1400\n",
            "hostname = 'valid'\nlisteners = [\ninstance_name = 'kept'\n[flags]\nmtu = 1400\n",
            "hostname = 'valid'\n[broken\nsecret = 'discard'\n[flags]\nmtu = 1400\n",
            "hostname = 'valid'\nsni = '''unclosed\ninstance_name = 'kept'\n[flags]\nmtu = 1400\n",
        ] {
            let config =
                TomlConfig::new_from_str_tolerant_with_source("broken.toml", input).unwrap();
            assert_eq!(config.get_hostname(), "valid", "{input}");
            assert_eq!(config.get_flags_patch().mtu, Some(1400), "{input}");
            if input.contains("instance_name") {
                assert_eq!(config.get_inst_name(), "kept", "{input}");
            }
        }
    }

    #[test]
    fn valid_multiline_toml_is_preserved() {
        let input = "hostname = '''hello\nworld'''\nlisteners = [\n 'tcp://0.0.0.0:11010',\n 'udp://0.0.0.0:11011',\n]\n";
        let (config, warnings) = load(input).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(config.listeners.unwrap().len(), 2);
    }

    #[test]
    fn managed_credentials_keep_valid_entries_and_redact_validation_warnings() {
        use crate::config::toml::ManagedCredentialConfig;
        use base64::Engine as _;

        let credential = |id: &str, byte| ManagedCredentialConfig {
            credential_id: id.to_owned(),
            credential_secret: base64::engine::general_purpose::STANDARD.encode([byte; 32]),
            groups: Vec::new(),
            allow_relay: false,
            allowed_proxy_cidrs: Vec::new(),
            expiry_unix: i64::MAX,
            reusable: true,
        };
        let valid_a = credential("valid-a", 1u8);
        let valid_b = credential("valid-b", 2u8);
        let mut bad_secret = credential("bad-secret", 3u8);
        bad_secret.credential_secret = "secret-that-must-not-appear-in-warnings".to_owned();
        let duplicate_key = credential("duplicate-key", 1u8);
        let duplicate_id = credential("valid-a", 4u8);
        let mut bad_cidr = credential("bad-cidr", 5u8);
        bad_cidr.allowed_proxy_cidrs = vec!["not-a-subnet".to_owned()];
        let mut input = "hostname = 'credential-test'\n".to_owned();
        for entry in [
            &valid_a,
            &bad_secret,
            &duplicate_key,
            &duplicate_id,
            &bad_cidr,
            &valid_b,
        ] {
            input.push_str("\n[[managed_credentials]]\n");
            input.push_str(&toml::to_string(entry).unwrap());
        }

        let (recovered, warnings) = load(&input).unwrap();
        assert_eq!(
            recovered.managed_credentials,
            [valid_a.clone(), valid_b.clone()]
        );
        assert_eq!(warnings.len(), 4);
        for (warning, index) in warnings.iter().zip(1..=4) {
            assert_eq!(warning.path, format!("managed_credentials[{index}]"));
            assert_eq!(
                warning.reason,
                "invalid managed credential or duplicate identity"
            );
            assert!(!warning.reason.contains(&bad_secret.credential_secret));
            assert!(!warning.path.contains(&bad_secret.credential_secret));
        }
        let recovered =
            TomlConfig::new_from_str_tolerant_with_source("credentials.toml", &input).unwrap();
        assert_eq!(recovered.get_managed_credentials(), [valid_a, valid_b]);
        assert_eq!(recovered.get_hostname(), "credential-test");
    }

    #[test]
    fn portal_reuses_runtime_validation_and_keeps_valid_clients() {
        use base64::Engine as _;
        let key = base64::engine::general_purpose::STANDARD.encode([8u8; 32]);
        let input = format!(
            r#"
ipv4 = "10.0.0.1/24"
[network_identity]
network_name = "portal-test"
network_secret = "secret"
[vpn_portal_config]
wireguard_listen = "0.0.0.0:22020"
wireguard_private_key = "{key}"
[[vpn_portal_config.clients]]
name = "client-a"
virtual_ip = "10.0.0.2/24"
[[vpn_portal_config.clients]]
name = "duplicate-address"
virtual_ip = "10.0.0.2/24"
[[vpn_portal_config.clients]]
name = "uses-host-address"
virtual_ip = "10.0.0.1/24"
[[vpn_portal_config.clients]]
name = "client-b"
virtual_ip = "10.0.0.3/24"
"#
        );
        let config = TomlConfig::new_from_str_tolerant_with_source("portal.toml", &input).unwrap();
        let portal = config.get_vpn_portal_config().unwrap();
        assert_ne!(portal.enabled, Some(false));
        assert_eq!(portal.clients.len(), 2);
        assert_eq!(portal.clients[0].name, "client-a");
        assert_eq!(portal.clients[1].name, "client-b");
    }

    #[test]
    fn unusable_portal_and_socks5_do_not_prevent_instance_normalization() {
        let input = r#"
hostname = "survives"
socks5_proxy = "socks5://example.com:1080"
[vpn_portal_config]
wireguard_listen = "0.0.0.0:22020"
wireguard_private_key = "invalid-private-key"
"#;
        let config = TomlConfig::new_from_str_tolerant_with_source("portal.toml", input).unwrap();
        assert_eq!(config.get_hostname(), "survives");
        assert_eq!(config.get_socks5_portal(), None);
        assert_eq!(config.get_vpn_portal_config().unwrap().enabled, Some(false));
        let runtime = crate::instance::CoreInstanceConfig::from_toml(&config).unwrap();
        assert!(runtime.vpn_portal.is_none());
    }
}
