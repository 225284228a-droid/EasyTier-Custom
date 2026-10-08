//! Field-scoped edits of the original persisted document. Conversion through
//! NetworkConfig supplies validation, but never replaces unselected TOML.

use std::collections::HashSet;

use anyhow::{Context as _, bail};
use easytier_proto::api::manage::NetworkConfig;
use serde_json::Value;
use toml_edit::{DocumentMut, Item, TableLike};

use crate::config::{
    api::network_config_from_toml,
    api_input::NetworkConfigExt as _,
    toml::{ConfigLoader as _, TomlConfig},
};

fn snake_case(name: &str) -> String {
    let mut output = String::new();
    for ch in name.chars() {
        if ch.is_ascii_uppercase() {
            output.push('_');
            output.push(ch.to_ascii_lowercase());
        } else {
            output.push(ch);
        }
    }
    output
}

fn normalized_json(value: Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (snake_case(&key), normalized_json(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(normalized_json).collect()),
        other => other,
    }
}

/// Paths in the typed API and the corresponding document locations. Several
/// UI fields intentionally describe one TOML property (e.g. address/prefix).
fn document_paths(field: &str) -> Option<Vec<&'static str>> {
    let paths = match field {
        "hostname" => vec!["hostname"],
        "sni" => vec!["sni"],
        "dhcp" => vec!["dhcp", "ipv4"],
        "virtual_ipv4" | "network_length" => vec!["ipv4"],
        "network_name" => vec!["network_identity.network_name"],
        "network_secret" => vec![
            "network_identity.network_secret",
            "network_identity.network_secret_digest",
        ],
        "networking_method" | "public_server_url" | "peer_urls" | "peers" => vec!["peer"],
        "proxy_cidrs" => vec!["proxy_network"],
        "listener_urls" => vec!["listeners"],
        "enable_manual_routes" | "routes" => vec!["routes"],
        "exit_nodes" => vec!["exit_nodes"],
        "enable_socks5" | "socks5_port" => vec!["socks5_proxy"],
        "mapped_listeners" => vec!["mapped_listeners"],
        "port_forwards" => vec!["port_forward"],
        "secure_mode" => vec!["secure_mode"],
        "acl" => vec!["acl"],
        "credential_file" => vec!["credential_file"],
        "managed_credentials" => vec!["managed_credentials"],
        "vpn_portal_config" => vec!["vpn_portal_config"],
        "ipv6_public_addr_provider" => vec!["ipv6_public_addr_provider"],
        "ipv6_public_addr_auto" => vec!["ipv6_public_addr_auto"],
        "ipv6_public_addr_prefix" => vec!["ipv6_public_addr_prefix"],
        "advanced_settings" => vec![],
        "disable_ipv6" => vec!["flags.enable_ipv6"],
        "disable_encryption" => vec!["flags.enable_encryption"],
        "enable_magic_dns" => vec!["flags.accept_dns"],
        "enable_private_mode" => vec!["flags.private_mode"],
        "enable_relay_network_whitelist" | "relay_network_whitelist" => {
            vec!["flags.relay_network_whitelist"]
        }
        "p2p_prefer_protocol" => vec!["flags.default_protocol"],
        "latency_first" => vec!["flags.latency_first"],
        "dev_name" => vec!["flags.dev_name"],
        "use_smoltcp" => vec!["flags.use_smoltcp"],
        "enable_kcp_proxy" => vec!["flags.enable_kcp_proxy"],
        "disable_kcp_input" => vec!["flags.disable_kcp_input"],
        "disable_p2p" => vec!["flags.disable_p2p"],
        "bind_device" => vec!["flags.bind_device"],
        "no_tun" => vec!["flags.no_tun"],
        "enable_exit_node" => vec!["flags.enable_exit_node"],
        "relay_all_peer_rpc" => vec!["flags.relay_all_peer_rpc"],
        "multi_thread" => vec!["flags.multi_thread"],
        "proxy_forward_by_system" => vec!["flags.proxy_forward_by_system"],
        "disable_udp_hole_punching" => vec!["flags.disable_udp_hole_punching"],
        "mtu" => vec!["flags.mtu"],
        "enable_quic_proxy" => vec!["flags.enable_quic_proxy"],
        "disable_quic_input" => vec!["flags.disable_quic_input"],
        "disable_sym_hole_punching" => vec!["flags.disable_sym_hole_punching"],
        "p2p_only" => vec!["flags.p2p_only"],
        "data_compress_algo" => vec!["flags.data_compress_algo"],
        "encryption_algorithm" => vec!["flags.encryption_algorithm"],
        "disable_tcp_hole_punching" => vec!["flags.disable_tcp_hole_punching"],
        "lazy_p2p" => vec!["flags.lazy_p2p"],
        "need_p2p" => vec!["flags.need_p2p"],
        "instance_recv_bps_limit" => vec!["flags.instance_recv_bps_limit"],
        "disable_upnp" => vec!["flags.disable_upnp"],
        "disable_relay_data" => vec!["flags.disable_relay_data"],
        "enable_udp_broadcast_relay" => vec!["flags.enable_udp_broadcast_relay"],
        "socket_mark" => vec!["flags.socket_mark"],
        "prefer_peer_relay" => vec!["flags.prefer_peer_relay"],
        "only_use_wss_http3_for_hole_punching" => {
            vec!["flags.only_use_wss_http3_for_hole_punching"]
        }
        "prefer_wss_http3_for_p2p" => vec!["flags.prefer_wss_http3_for_p2p"],
        "disable_wss_http3_for_p2p" => vec!["flags.disable_wss_http3_for_p2p"],
        "enable_bbr" => vec!["flags.enable_bbr"],
        "close_redundant_conns_when_disguised" => {
            vec!["flags.close_redundant_conns_when_disguised"]
        }
        _ => return None,
    };
    Some(paths)
}

pub(super) fn required_capability(field: &str) -> Option<String> {
    match field.split('.').next().unwrap_or_default() {
        field @ ("sni"
        | "enable_bbr"
        | "p2p_prefer_protocol"
        | "only_use_wss_http3_for_hole_punching"
        | "prefer_wss_http3_for_p2p"
        | "disable_wss_http3_for_p2p"
        | "close_redundant_conns_when_disguised") => Some(format!("config:{field}")),
        _ => None,
    }
}

fn update_json(target: &mut Value, source: &Value, path: &[&str]) -> anyhow::Result<()> {
    let object = target
        .as_object_mut()
        .context("field mask traverses a non-object")?;
    if path.len() == 1 {
        match source.get(path[0]) {
            Some(value) => {
                object.insert(path[0].to_owned(), value.clone());
            }
            None => {
                object.remove(path[0]);
            }
        }
        return Ok(());
    }
    let child = object
        .entry(path[0])
        .or_insert_with(|| Value::Object(Default::default()));
    update_json(
        child,
        source.get(path[0]).unwrap_or(&Value::Null),
        &path[1..],
    )
}

fn allowed_nested_path(path: &str) -> bool {
    matches!(
        path,
        "secure_mode.enabled"
            | "secure_mode.local_private_key"
            | "secure_mode.local_public_key"
            | "vpn_portal_config.enabled"
            | "vpn_portal_config.wireguard_listen"
            | "vpn_portal_config.wireguard_private_key"
            | "vpn_portal_config.clients"
            | "acl.acl_v1"
            | "acl.acl_v1.chains"
            | "acl.acl_v1.group"
            | "acl.acl_v1.group.declares"
            | "acl.acl_v1.group.members"
    )
}

/// Validate only selected values. Existing unselected transports can remain
/// on disk even on a restricted build; new edits require registered support.
pub(super) fn unsupported_capability(
    values: &NetworkConfig,
    fields: &[String],
    caps: &[String],
) -> Option<String> {
    fn transport_capability(uri: &str) -> Option<&'static str> {
        match uri
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "ws" => Some("transport:ws"),
            "wss" => Some("transport:wss"),
            "http3" => Some("transport:http3-framed-v1"),
            _ => None,
        }
    }
    for field in fields {
        if let Some(required) = required_capability(field)
            && !caps.contains(&required)
        {
            return Some(required);
        }
        let uris = match field.as_str() {
            "peer_urls" => values
                .peer_urls
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            "peers" => values.peers.iter().map(|peer| peer.uri.as_str()).collect(),
            "listener_urls" => values.listener_urls.iter().map(String::as_str).collect(),
            "mapped_listeners" => values.mapped_listeners.iter().map(String::as_str).collect(),
            "public_server_url" => values
                .public_server_url
                .iter()
                .map(String::as_str)
                .collect(),
            _ => Vec::new(),
        };
        for uri in uris {
            if let Some(required) = transport_capability(uri.trim())
                && !caps.iter().any(|cap| cap == required)
            {
                return Some(required.to_owned());
            }
        }
    }
    None
}

fn get_item<'a>(document: &'a DocumentMut, path: &str) -> Option<&'a Item> {
    let mut keys = path.split('.');
    let mut item = document.get(keys.next()?)?;
    for key in keys {
        item = item.get(key)?;
    }
    Some(item)
}

fn set_item(document: &mut DocumentMut, path: &str, value: Option<Item>) {
    fn write_path(table: &mut dyn TableLike, keys: &[&str], value: Option<Item>) {
        if keys.len() == 1 {
            match value {
                Some(value) => {
                    table.insert(keys[0], value);
                }
                None => {
                    table.remove(keys[0]);
                }
            }
        } else {
            if value.is_some() && table.get(keys[0]).and_then(Item::as_table_like).is_none() {
                table.insert(keys[0], Item::Table(Default::default()));
            }
            if let Some(child) = table.get_mut(keys[0]).and_then(Item::as_table_like_mut) {
                write_path(child, &keys[1..], value);
            }
        }
    }
    write_path(
        document.as_table_mut(),
        &path.split('.').collect::<Vec<_>>(),
        value,
    );
}

/// Validate a typed field mask, apply values against the current persisted
/// projection, and copy only the selected properties into the original AST.
pub(super) fn merge_persisted_config(
    original: &str,
    values: &NetworkConfig,
    field_mask: &[String],
) -> anyhow::Result<String> {
    if field_mask.is_empty() {
        bail!("field_mask must not be empty");
    }
    if field_mask
        .iter()
        .any(|field| field == "p2p_prefer_protocol")
        && values
            .p2p_prefer_protocol
            .as_deref()
            .is_some_and(|protocol| !matches!(protocol, "" | "tcp" | "udp"))
    {
        bail!("invalid P2P preferred protocol");
    }
    let loaded = TomlConfig::new_from_str(original)
        .map_err(|_| anyhow::anyhow!("invalid persisted TOML"))?;
    let mut base = normalized_json(serde_json::to_value(network_config_from_toml(&loaded))?);
    let values = normalized_json(serde_json::to_value(values)?);
    let mut selected = HashSet::new();
    let mut locations = HashSet::new();
    for path in field_mask {
        if path.is_empty()
            || path.split('.').any(|part| {
                part.is_empty()
                    || !part
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
            })
        {
            bail!("invalid field mask path");
        }
        if !selected.insert(path.as_str()) {
            bail!("duplicate field mask path");
        }
        let parts = path.split('.').collect::<Vec<_>>();
        let mappings = document_paths(parts[0]).context("field is not editable")?;
        if parts.len() > 1 && !allowed_nested_path(path) {
            bail!("field mask cannot traverse this field");
        }
        update_json(&mut base, &values, &parts)?;
        if parts.len() > 1 {
            // Nested typed edits preserve sibling settings in the same table.
            // SecureMode and Portal field names match their TOML names.
            locations.insert(path.clone());
        } else {
            locations.extend(mappings.into_iter().map(str::to_owned));
        }
    }
    for path in &selected {
        if selected
            .iter()
            .any(|other| *other != *path && other.starts_with(&format!("{path}.")))
        {
            bail!("overlapping field mask paths");
        }
    }
    // URI-only edits must not be silently ignored because the existing typed
    // peers representation takes precedence in NetworkConfig.gen_config().
    if selected.contains("peer_urls") && !selected.contains("peers") {
        // Keep public keys of unchanged URI entries when only the URI form
        // field was edited. A URI edit must not reset their authentication.
        let old_peers = network_config_from_toml(&loaded).peers;
        let peers = base
            .get("peer_urls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|uri| {
                old_peers
                    .iter()
                    .find(|peer| peer.uri == uri)
                    .map(|peer| serde_json::to_value(peer).unwrap())
                    .unwrap_or_else(|| serde_json::json!({"uri": uri}))
            })
            .collect::<Vec<_>>();
        base.as_object_mut()
            .unwrap()
            .insert("peers".into(), Value::Array(peers));
    }
    if selected.contains("public_server_url") && !selected.contains("peers") {
        base.as_object_mut().unwrap().remove("peers");
    }
    let merged: NetworkConfig =
        serde_json::from_value(base).map_err(|_| anyhow::anyhow!("invalid field mask values"))?;
    let generated = merged
        .gen_config()
        .map_err(|_| anyhow::anyhow!("invalid configuration values"))?
        .dump();
    let generated = generated.parse::<DocumentMut>()?;
    let mut original = original.parse::<DocumentMut>()?;
    for path in locations {
        set_item(&mut original, &path, get_item(&generated, &path).cloned());
    }
    let result = original.to_string();
    TomlConfig::new_from_str(&result)
        .map_err(|_| anyhow::anyhow!("invalid resulting configuration"))?;
    Ok(result)
}

/// Hot patches are generated from active configuration. Preserve unrelated
/// persisted-only edits by copying only properties changed by this operation.
pub(crate) fn merge_active_changes(
    original: &str,
    before: &str,
    after: &str,
) -> anyhow::Result<String> {
    let mut original = original
        .parse::<DocumentMut>()
        .map_err(|_| anyhow::anyhow!("invalid persisted configuration"))?;
    let before = before
        .parse::<DocumentMut>()
        .map_err(|_| anyhow::anyhow!("invalid active configuration"))?;
    let after = after
        .parse::<DocumentMut>()
        .map_err(|_| anyhow::anyhow!("invalid resulting configuration"))?;
    fn collect_changes(
        before: Option<&Item>,
        after: Option<&Item>,
        path: String,
        changes: &mut Vec<String>,
    ) {
        if let (Some(before), Some(after)) = (
            before.and_then(Item::as_table_like),
            after.and_then(Item::as_table_like),
        ) {
            let keys = before
                .iter()
                .chain(after.iter())
                .map(|(key, _)| key.to_owned())
                .collect::<HashSet<_>>();
            for key in keys {
                collect_changes(
                    before.get(&key),
                    after.get(&key),
                    format!("{path}.{key}"),
                    changes,
                );
            }
        } else if before.map(ToString::to_string) != after.map(ToString::to_string) {
            changes.push(path);
        }
    }
    let top_keys = before
        .iter()
        .chain(after.iter())
        .map(|(key, _)| key.to_owned())
        .collect::<HashSet<_>>();
    let mut changes = Vec::new();
    for key in top_keys {
        collect_changes(before.get(&key), after.get(&key), key, &mut changes);
    }
    for path in changes {
        set_item(&mut original, &path, get_item(&after, &path).cloned());
    }
    Ok(original.to_string())
}

/// Legacy clients provide an entire form without a dirty mask. Retain their
/// established form semantics while preserving original non-form TOML and
/// comments that neither typed form projection contains.
pub(super) fn merge_legacy_form_changes(
    original: &str,
    candidate: &TomlConfig,
) -> anyhow::Result<String> {
    let original_config = TomlConfig::new_from_str(original)
        .map_err(|_| anyhow::anyhow!("invalid persisted configuration"))?;
    let before = network_config_from_toml(&original_config)
        .gen_config()
        .map_err(|_| anyhow::anyhow!("persisted configuration cannot be edited"))?;
    before.set_network_config_source(Some(original_config.get_network_config_source()));
    merge_active_changes(original, &before.dump(), &candidate.dump())
}

#[cfg(test)]
mod tests {
    use super::*;
    const ORIGINAL: &str = "# retained comment\ninstance_id = '11111111-1111-1111-1111-111111111111'\nhostname = 'old'\nnetns = 'keep'\nstun_servers = ['udp://stun.example:3478']\n[network_identity]\nnetwork_name = 'home'\nnetwork_secret = 'secret'\n[flags]\ndefault_protocol = 'udp'\nmtu = 1200\n";

    #[test]
    fn malformed_persisted_files_do_not_include_secret_snippets_in_errors() {
        let invalid = "network_secret = [ private-secret-value";
        let error = merge_active_changes(invalid, ORIGINAL, ORIGINAL).unwrap_err();
        assert_eq!(error.to_string(), "invalid persisted configuration");
        let error = merge_legacy_form_changes(invalid, &TomlConfig::default()).unwrap_err();
        assert_eq!(error.to_string(), "invalid persisted configuration");
    }

    #[test]
    fn typed_edit_preserves_comments_unknown_fields_and_unselected_flags() {
        let merged = merge_persisted_config(
            ORIGINAL,
            &NetworkConfig {
                hostname: Some("new".into()),
                ..Default::default()
            },
            &["hostname".into()],
        )
        .unwrap();
        assert!(merged.starts_with("# retained comment"));
        assert!(merged.contains("netns = 'keep'"));
        assert!(merged.contains("stun_servers = ['udp://stun.example:3478']"));
        assert!(merged.contains("default_protocol = 'udp'"));
        assert_eq!(
            TomlConfig::new_from_str(&merged).unwrap().get_hostname(),
            "new"
        );
    }

    #[test]
    fn explicit_false_reset_is_not_a_missing_value() {
        let original = format!("{ORIGINAL}\ndisable_p2p = true\n");
        let merged = merge_persisted_config(
            &original,
            &NetworkConfig {
                disable_p2p: Some(false),
                ..Default::default()
            },
            &["disable_p2p".into()],
        )
        .unwrap();
        assert!(
            !TomlConfig::new_from_str(&merged)
                .unwrap()
                .get_flags()
                .disable_p2p
        );
        assert!(merged.contains("mtu = 1200"));
    }

    #[test]
    fn masks_cannot_reidentify_or_reset_unselected_fields() {
        assert!(
            merge_persisted_config(ORIGINAL, &NetworkConfig::default(), &["instance_id".into()])
                .is_err()
        );
        assert!(
            merge_persisted_config(
                ORIGINAL,
                &NetworkConfig::default(),
                &["secure_mode".into(), "secure_mode.enabled".into()]
            )
            .is_err()
        );
        assert!(
            merge_persisted_config(
                ORIGINAL,
                &NetworkConfig::default(),
                &["hostname.nope".into()]
            )
            .is_err()
        );
        assert!(
            merge_persisted_config(ORIGINAL, &NetworkConfig::default(), &["acl.unknown".into()])
                .is_err()
        );
    }

    #[test]
    fn hot_patch_keeps_unrelated_persisted_only_edits() {
        let pending = ORIGINAL.replace("hostname = 'old'", "hostname = 'pending'");
        let after = ORIGINAL.replace("mtu = 1200", "mtu = 1300");
        let merged = merge_active_changes(&pending, ORIGINAL, &after).unwrap();
        assert!(merged.contains("hostname = 'pending'"));
        assert!(merged.contains("mtu = 1300"));
        assert!(merged.contains("# retained comment"));
    }

    #[test]
    fn capability_checks_apply_only_to_masked_values() {
        let values = NetworkConfig {
            peer_urls: vec!["http3://example.com:443".into()],
            sni: Some("name".into()),
            ..Default::default()
        };
        assert!(unsupported_capability(&values, &["hostname".into()], &[]).is_none());
        assert_eq!(
            unsupported_capability(&values, &["peer_urls".into()], &[]).as_deref(),
            Some("transport:http3-framed-v1")
        );
        assert!(
            unsupported_capability(
                &values,
                &["peer_urls".into()],
                &["transport:http3-framed-v1".into()]
            )
            .is_none()
        );
        assert_eq!(
            unsupported_capability(&values, &["sni".into()], &[]).as_deref(),
            Some("config:sni")
        );
    }

    #[test]
    fn legacy_typed_save_retains_non_form_properties_and_comments() {
        let mut form = network_config_from_toml(&TomlConfig::new_from_str(ORIGINAL).unwrap());
        form.hostname = Some("legacy-edit".into());
        let merged = merge_legacy_form_changes(ORIGINAL, &form.gen_config().unwrap()).unwrap();
        assert!(merged.contains("netns = 'keep'"));
        assert!(merged.contains("stun_servers = ['udp://stun.example:3478']"));
        assert!(merged.contains("# retained comment"));
        assert_eq!(
            TomlConfig::new_from_str(&merged).unwrap().get_hostname(),
            "legacy-edit"
        );
    }
}
