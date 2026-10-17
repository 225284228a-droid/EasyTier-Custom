//! Field-scoped edits of the original persisted document. Conversion through
//! NetworkConfig supplies validation, but never replaces unselected TOML.

use std::collections::HashSet;

use anyhow::Context as _;
use easytier_proto::api::manage::NetworkConfig;
use serde_json::Value;
use toml_edit::{DocumentMut, Item, TableLike};

use crate::config::{
    api::network_config_from_toml,
    api_input::NetworkConfigExt as _,
    toml::{ConfigLoader as _, TomlConfig},
};

const LEGACY_DISGUISE_FIELDS: [&str; 3] = [
    "prefer_wss_http3_for_p2p",
    "disable_wss_http3_for_p2p",
    "only_use_wss_http3_for_hole_punching",
];

/// Only fixed reasons and validated field names may cross the management RPC.
/// Conversion errors can contain configuration values, URLs or key material.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct PatchDiagnostic {
    message: String,
}

fn invalid_patch<'a>(
    reason: &'static str,
    fields: impl IntoIterator<Item = &'a str>,
) -> anyhow::Error {
    let mut names = Vec::new();
    for field in fields {
        if field.len() <= 128
            && field.split('.').all(|part| {
                part.starts_with(|ch: char| ch.is_ascii_lowercase())
                    && part
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
            })
            && !names.contains(&field)
        {
            names.push(field);
        }
    }
    PatchDiagnostic {
        message: if names.is_empty() {
            reason.to_owned()
        } else {
            format!("{reason} (fields: {})", names.join(", "))
        },
    }
    .into()
}

pub(super) fn public_patch_error(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<PatchDiagnostic>()
        .map(ToString::to_string)
        .unwrap_or_else(|| "invalid field mask or configuration values".to_owned())
}

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
        "parallel_connections" => vec!["parallel_connections"],
        "disable_parallel_connections" => vec!["disable_parallel_connections"],
        "traffic_disguise" => vec!["traffic_disguise"],
        "dhcp" => vec!["dhcp", "ipv4"],
        "dhcp_ipv4_cidr" => vec!["dhcp"],
        "dhcpv6" | "dhcp_ipv6_cidr" => vec!["dhcpv6"],
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
        "vpn" => vec!["vpn"],
        "enable_socks5" | "socks5_port" => vec!["socks5_proxy"],
        "mapped_listeners" => vec!["mapped_listeners"],
        "port_forwards" => vec!["port_forward"],
        "secure_mode" => vec!["secure_mode"],
        "acl" => vec!["acl"],
        "credential_file" => vec!["credential_file"],
        "managed_credentials" => vec!["managed_credentials"],
        "vpn_portal_config" => vec!["vpn_portal_config"],
        "advanced_settings" => vec![],
        "enable_relay_network_whitelist" | "relay_network_whitelist" => {
            vec!["flags.relay_network_whitelist"]
        }
        "p2p_disguise_mode" => vec![
            "flags.p2p_disguise_mode",
            "flags.prefer_wss_http3_for_p2p",
            "flags.disable_wss_http3_for_p2p",
            "flags.only_use_wss_http3_for_hole_punching",
        ],
        _ => return easytier_proto::common::Flags::api_document_path(field).map(|path| vec![path]),
    };
    Some(paths)
}

pub(super) fn required_capability(field: &str) -> Option<String> {
    NetworkConfig::required_capability(field).map(ToOwned::to_owned)
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
            "http3" => Some("transport:http3-rfc9114-v2"),
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
    // Flags accept protobuf camelCase aliases when reading. A selected reset
    // must remove that spelling too, or an omitted default would leave the
    // old alias active (and a non-default write could create duplicate keys).
    if let Some(name) = path.strip_prefix("flags.")
        && let Some(field) = easytier_proto::common::Flags::flags()
            .iter()
            .find(|field| field.name.as_deref() == Some(name))
        && let Some(alias) = field.json_name.as_deref().filter(|alias| *alias != name)
        && let Some(flags) = document.get_mut("flags").and_then(Item::as_table_like_mut)
    {
        flags.remove(alias);
    }
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
            if table
                .get(keys[0])
                .and_then(Item::as_table_like)
                .is_some_and(|child| child.is_empty())
            {
                table.remove(keys[0]);
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
        return Err(invalid_patch("field_mask must not be empty", []));
    }
    if field_mask
        .iter()
        .any(|field| field == "p2p_prefer_protocol")
        && values
            .p2p_prefer_protocol
            .as_deref()
            .is_some_and(|protocol| !matches!(protocol, "" | "tcp" | "udp"))
    {
        return Err(invalid_patch(
            "invalid P2P preferred protocol; expected tcp or udp",
            ["p2p_prefer_protocol"],
        ));
    }
    let loaded = TomlConfig::new_from_str(original)
        .map_err(|_| invalid_patch("invalid persisted TOML", []))?;
    let fields = || field_mask.iter().map(String::as_str);
    let mut base = normalized_json(
        serde_json::to_value(network_config_from_toml(&loaded))
            .map_err(|_| invalid_patch("invalid persisted configuration values", []))?,
    );
    let legacy_disguise_edit = !field_mask.iter().any(|field| field == "p2p_disguise_mode")
        && field_mask
            .iter()
            .any(|field| LEGACY_DISGUISE_FIELDS.contains(&field.as_str()))
        && loaded
            .get_flags_patch()
            .p2p_disguise_mode
            .as_deref()
            .is_some_and(|mode| !mode.trim().is_empty());
    if legacy_disguise_edit {
        // An older client edits the boolean projection of a unified policy.
        // Start from its effective values, so changing one flag preserves the
        // other two, then retire the mode that would otherwise override it.
        let flags = loaded.get_flags();
        let object = base.as_object_mut().unwrap();
        object.remove("p2p_disguise_mode");
        for (field, value) in [
            ("prefer_wss_http3_for_p2p", flags.prefer_wss_http3_for_p2p),
            ("disable_wss_http3_for_p2p", flags.disable_wss_http3_for_p2p),
            (
                "only_use_wss_http3_for_hole_punching",
                flags.only_use_wss_http3_for_hole_punching,
            ),
        ] {
            object.insert(field.into(), Value::Bool(value));
        }
    }
    let values = normalized_json(
        serde_json::to_value(values)
            .map_err(|_| invalid_patch("invalid configuration values", fields()))?,
    );
    let mut selected = HashSet::new();
    let mut locations = HashSet::new();
    if legacy_disguise_edit {
        locations.insert("flags.p2p_disguise_mode".to_owned());
        locations.extend(LEGACY_DISGUISE_FIELDS.map(|field| format!("flags.{field}")));
    }
    for path in field_mask {
        if path.is_empty()
            || path.split('.').any(|part| {
                part.is_empty()
                    || !part
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
            })
        {
            return Err(invalid_patch("invalid field mask path", []));
        }
        if !selected.insert(path.as_str()) {
            return Err(invalid_patch("duplicate field mask path", [path.as_str()]));
        }
        let parts = path.split('.').collect::<Vec<_>>();
        let mappings = document_paths(parts[0])
            .ok_or_else(|| invalid_patch("field is not editable", [path.as_str()]))?;
        if parts.len() > 1 && !allowed_nested_path(path) {
            return Err(invalid_patch(
                "field mask cannot traverse this field",
                [path.as_str()],
            ));
        }
        update_json(&mut base, &values, &parts)
            .map_err(|_| invalid_patch("field mask traverses a non-object", [path.as_str()]))?;
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
            return Err(invalid_patch("overlapping field mask paths", [*path]));
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
    if selected.contains("p2p_disguise_mode") {
        // Selecting/resetting the unified policy replaces legacy overrides,
        // including when the selected default is omitted from the payload.
        for field in LEGACY_DISGUISE_FIELDS {
            base.as_object_mut().unwrap().remove(field);
        }
    }
    let merged: NetworkConfig = serde_json::from_value(base)
        .map_err(|_| invalid_patch("invalid field mask values", fields()))?;
    let generated = merged
        .gen_config()
        .map_err(|_| invalid_patch("invalid configuration values", fields()))?
        .dump();
    let generated = generated
        .parse::<DocumentMut>()
        .map_err(|_| invalid_patch("invalid resulting configuration", fields()))?;
    let mut original = original
        .parse::<DocumentMut>()
        .map_err(|_| invalid_patch("invalid persisted TOML", []))?;
    for path in locations {
        set_item(&mut original, &path, get_item(&generated, &path).cloned());
    }
    let result = original.to_string();
    TomlConfig::new_from_str(&result)
        .map_err(|_| invalid_patch("invalid resulting configuration", fields()))?;
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
    let candidate_toml = candidate.dump();
    let merged = merge_active_changes(original, &before.dump(), &candidate_toml)?;
    let generated = candidate_toml.parse::<DocumentMut>()?;
    let mut merged = merged.parse::<DocumentMut>()?;
    // A full form save owns these defaults even when normalization made the
    // before/after projections identical. Remove old explicit defaults too,
    // while retaining flags and tables that the form cannot represent.
    for flag in easytier_proto::common::Flags::form() {
        let path = format!("flags.{flag}");
        if get_item(&generated, &path).is_none() {
            set_item(&mut merged, &path, None);
        }
    }
    for path in [
        "hostname",
        "sni",
        "parallel_connections",
        "disable_parallel_connections",
        "traffic_disguise",
        "dhcp",
        "dhcpv6",
        "vpn",
        "secure_mode",
        "mapped_listeners",
    ] {
        if get_item(&generated, path).is_none() {
            set_item(&mut merged, path, None);
        }
    }
    Ok(merged.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    const ORIGINAL: &str = "# retained comment\ninstance_id = '11111111-1111-1111-1111-111111111111'\nhostname = 'old'\nnetns = 'keep'\nstun_servers = ['udp://stun.example:3478']\n[network_identity]\nnetwork_name = 'home'\nnetwork_secret = 'secret'\n[flags]\ndefault_protocol = 'udp'\nmtu = 1200\n";

    #[test]
    fn parallel_patch_keeps_count_on_disable_and_removes_explicit_defaults() {
        let original = format!(
            "parallel_connections = 5\ndisable_parallel_connections = true\ntraffic_disguise = 'video'\n{ORIGINAL}"
        );
        let merged = merge_persisted_config(
            &original,
            &NetworkConfig {
                disable_parallel_connections: Some(false),
                ..Default::default()
            },
            &["disable_parallel_connections".into()],
        )
        .unwrap();
        let config = TomlConfig::new_from_str(&merged).unwrap();
        assert_eq!(config.get_parallel_connections(), 5);
        assert!(!config.get_disable_parallel_connections());
        assert_eq!(config.get_traffic_disguise(), "video");
        assert!(merged.contains("# retained comment"));
        let merged = merge_persisted_config(
            &merged,
            &NetworkConfig {
                parallel_connections: Some(1),
                traffic_disguise: Some("speedtest".into()),
                ..Default::default()
            },
            &["parallel_connections".into(), "traffic_disguise".into()],
        )
        .unwrap();
        for key in [
            "parallel_connections",
            "disable_parallel_connections",
            "traffic_disguise",
        ] {
            assert!(!merged.contains(key));
        }
        assert_eq!(
            TomlConfig::new_from_str(&merged).unwrap().get_flags().mtu,
            1200
        );
    }

    #[test]
    fn generated_flag_paths_keep_compound_document_ownership() {
        assert_eq!(
            document_paths("p2p_disguise_mode").unwrap(),
            vec![
                "flags.p2p_disguise_mode",
                "flags.prefer_wss_http3_for_p2p",
                "flags.disable_wss_http3_for_p2p",
                "flags.only_use_wss_http3_for_hole_punching",
            ]
        );
        for (fields, path) in [
            (
                ["enable_relay_network_whitelist", "relay_network_whitelist"],
                "flags.relay_network_whitelist",
            ),
            (["enable_manual_routes", "routes"], "routes"),
            (["enable_socks5", "socks5_port"], "socks5_proxy"),
        ] {
            for field in fields {
                assert_eq!(document_paths(field).unwrap(), vec![path], "{field}");
            }
        }
    }

    #[test]
    fn generated_flag_paths_preserve_alias_negation_and_explicit_false_or_unset() {
        let original = format!(
            "{ORIGINAL}enable_ipv6 = false\nenable_encryption = false\nprivate_mode = false\nmulti_thread_count = 7\nclose_redundant_conns_when_disguised = false\n"
        );
        for value in [Some(true), Some(false), None] {
            let merged = merge_persisted_config(
                &original,
                &NetworkConfig {
                    p2p_prefer_protocol: Some("tcp".into()),
                    disable_ipv6: value,
                    disable_encryption: value,
                    enable_private_mode: value,
                    ..Default::default()
                },
                &[
                    "p2p_prefer_protocol".into(),
                    "disable_ipv6".into(),
                    "disable_encryption".into(),
                    "enable_private_mode".into(),
                ],
            )
            .unwrap();
            let loaded = TomlConfig::new_from_str(&merged).unwrap();
            let flags = loaded.get_flags();
            assert_eq!(flags.default_protocol, "tcp");
            assert_eq!(flags.enable_ipv6, !value.unwrap_or(false));
            assert_eq!(flags.enable_encryption, !value.unwrap_or(false));
            assert_eq!(flags.private_mode, value.unwrap_or(true));
            assert!(!flags.close_redundant_conns_when_disguised);
            let patch = loaded.get_flags_patch();
            assert_eq!(patch.enable_ipv6, (value == Some(true)).then_some(false));
            assert_eq!(
                patch.enable_encryption,
                (value == Some(true)).then_some(false)
            );
            assert_eq!(patch.private_mode, (value == Some(false)).then_some(false));
            assert!(merged.contains("multi_thread_count = 7"));
            assert!(merged.contains("mtu = 1200"));
            assert!(merged.contains("# retained comment"));
        }
    }

    #[test]
    fn generated_flag_paths_preserve_whitelist_enable_and_list_edits() {
        let original = format!("{ORIGINAL}relay_network_whitelist = 'home work'\n");
        let list_edit = merge_persisted_config(
            &original,
            &NetworkConfig {
                relay_network_whitelist: vec!["new".into()],
                ..Default::default()
            },
            &["relay_network_whitelist".into()],
        )
        .unwrap();
        assert_eq!(
            TomlConfig::new_from_str(&list_edit)
                .unwrap()
                .get_flags()
                .relay_network_whitelist,
            "new"
        );
        let disabled = merge_persisted_config(
            &original,
            &NetworkConfig {
                enable_relay_network_whitelist: Some(false),
                ..Default::default()
            },
            &["enable_relay_network_whitelist".into()],
        )
        .unwrap();
        assert_eq!(
            TomlConfig::new_from_str(&disabled)
                .unwrap()
                .get_flags()
                .relay_network_whitelist,
            "*"
        );
        assert!(!disabled.contains("relay_network_whitelist ="));
        assert!(disabled.contains("mtu = 1200"));
        assert!(disabled.contains("# retained comment"));
    }

    #[test]
    fn dhcp_cidr_patches_preserve_unselected_toml_and_custom_policy() {
        let original = format!(
            "dhcp = '10.50.0.0/24'\ndhcpv6 = 'fd50::/64'\nipv4 = '10.99.0.2/24'\nipv6 = 'fd99::2/64'\n{ORIGINAL}p2p_disguise_mode = 'only'\n"
        );
        let patched = merge_persisted_config(
            &original,
            &NetworkConfig {
                dhcp_ipv4_cidr: Some("10.51.0.0/24".into()),
                dhcp_ipv6_cidr: Some("fd51::/64".into()),
                ..Default::default()
            },
            &["dhcp_ipv4_cidr".into(), "dhcp_ipv6_cidr".into()],
        )
        .unwrap();
        let before = TomlConfig::new_from_str(&original).unwrap();
        let after = TomlConfig::new_from_str(&patched).unwrap();
        assert_eq!(
            after.get_dhcp_config().cidr().unwrap().to_string(),
            "10.51.0.0/24"
        );
        assert_eq!(
            after.get_dhcpv6_config().cidr().unwrap().to_string(),
            "fd51::/64"
        );
        assert_eq!(after.get_ipv4(), before.get_ipv4());
        assert_eq!(after.get_ipv6(), before.get_ipv6());
        assert_eq!(after.get_flags(), before.get_flags());
        assert!(patched.contains("# retained comment"));
        assert!(patched.contains("netns = 'keep'"));

        let disabled = merge_persisted_config(
            &patched,
            &NetworkConfig {
                dhcpv6: Some(false),
                ..Default::default()
            },
            &["dhcpv6".into()],
        )
        .unwrap();
        let disabled = TomlConfig::new_from_str(&disabled).unwrap();
        assert!(!disabled.get_dhcpv6());
        assert_eq!(disabled.get_ipv6(), before.get_ipv6());
        assert_eq!(disabled.get_dhcp_config(), after.get_dhcp_config());
        assert_eq!(disabled.get_flags(), before.get_flags());
    }

    #[test]
    fn legacy_disguise_patch_edits_effective_mode_flags_without_losing_siblings() {
        let original = format!("{ORIGINAL}p2p_disguise_mode = 'disabled'\n");
        let changed = merge_persisted_config(
            &original,
            &NetworkConfig {
                disable_wss_http3_for_p2p: Some(false),
                ..Default::default()
            },
            &["disable_wss_http3_for_p2p".into()],
        )
        .unwrap();
        let flags = TomlConfig::new_from_str(&changed).unwrap().get_flags();
        assert!(!flags.disable_wss_http3_for_p2p);
        assert!(!flags.prefer_wss_http3_for_p2p);
        assert!(!flags.only_use_wss_http3_for_hole_punching);
        assert!(!changed.contains("p2p_disguise_mode ="));

        let original = format!("{ORIGINAL}p2p_disguise_mode = 'only'\n");
        let changed = merge_persisted_config(
            &original,
            &NetworkConfig {
                prefer_wss_http3_for_p2p: Some(true),
                ..Default::default()
            },
            &["prefer_wss_http3_for_p2p".into()],
        )
        .unwrap();
        let flags = TomlConfig::new_from_str(&changed).unwrap().get_flags();
        assert!(flags.prefer_wss_http3_for_p2p);
        assert!(flags.only_use_wss_http3_for_hole_punching);
        assert!(!flags.disable_wss_http3_for_p2p);
        assert!(!changed.contains("p2p_disguise_mode ="));
        assert!(changed.contains("# retained comment"));
    }

    #[test]
    fn unified_disguise_patch_replaces_legacy_flags_and_reset_uses_default() {
        let original = format!(
            "{ORIGINAL}prefer_wss_http3_for_p2p = false\nonly_use_wss_http3_for_hole_punching = true\n"
        );
        let mask = ["p2p_disguise_mode".to_owned()];
        let merged = merge_persisted_config(
            &original,
            &NetworkConfig {
                p2p_disguise_mode: Some("off".into()),
                ..Default::default()
            },
            &mask,
        )
        .unwrap();
        let flags = TomlConfig::new_from_str(&merged).unwrap().get_flags();
        assert!(!flags.prefer_wss_http3_for_p2p);
        assert!(!flags.only_use_wss_http3_for_hole_punching);
        assert!(merged.contains("# retained comment"));
        assert!(!merged.contains("prefer_wss_http3_for_p2p ="));
        assert!(!merged.contains("only_use_wss_http3_for_hole_punching ="));

        // An omitted selected value resets the whole policy, including old overrides.
        let reset = merge_persisted_config(&original, &NetworkConfig::default(), &mask).unwrap();
        let flags = TomlConfig::new_from_str(&reset).unwrap().get_flags();
        assert!(flags.prefer_wss_http3_for_p2p);
        assert!(!flags.only_use_wss_http3_for_hole_punching);
        assert!(!reset.contains("p2p_disguise_mode ="));
        assert!(!reset.contains("only_use_wss_http3_for_hole_punching ="));
    }

    #[test]
    fn full_form_save_removes_explicit_defaults_and_keeps_unmanaged_values() {
        let original = format!(
            "{ORIGINAL}private_mode = true\nlatency_first = false\nmulti_thread_count = 7\n[vpn]\nexit_enabled = true\nrules = []\n"
        );
        let loaded = TomlConfig::new_from_str(&original).unwrap();
        let form = network_config_from_toml(&loaded);
        let merged = merge_legacy_form_changes(&original, &form.gen_config().unwrap()).unwrap();
        assert!(merged.starts_with("# retained comment"));
        let document: toml::Table = toml::from_str(&merged).unwrap();
        assert!(!document.contains_key("vpn"));
        let flags = document["flags"].as_table().unwrap();
        assert!(!flags.contains_key("default_protocol"));
        assert!(!flags.contains_key("private_mode"));
        assert!(!flags.contains_key("latency_first"));
        assert_eq!(flags["multi_thread_count"].as_integer(), Some(7));
        assert_eq!(flags["mtu"].as_integer(), Some(1200));
        assert_eq!(
            TomlConfig::new_from_str(&merged).unwrap().get_flags(),
            loaded.get_flags()
        );
    }

    #[test]
    fn selected_reset_removes_default_and_empty_flags_table() {
        let original = "[flags]\nprivate_mode = false\n";
        let merged = merge_persisted_config(
            original,
            &NetworkConfig {
                enable_private_mode: Some(true),
                ..Default::default()
            },
            &["enable_private_mode".into()],
        )
        .unwrap();
        assert!(!merged.contains("[flags]"));
        assert!(
            TomlConfig::new_from_str(&merged)
                .unwrap()
                .get_flags()
                .private_mode
        );
    }

    #[test]
    fn selected_flag_reset_removes_accepted_camel_case_alias() {
        let original = "# retained\n[flags]\ndisableP2p = true\nmultiThreadCount = 7\n";
        let merged = merge_persisted_config(
            original,
            &NetworkConfig {
                disable_p2p: Some(false),
                ..Default::default()
            },
            &["disable_p2p".into()],
        )
        .unwrap();
        assert!(!merged.contains("disableP2p"));
        assert!(merged.contains("multiThreadCount = 7"));
        let parsed = TomlConfig::new_from_str(&merged).unwrap();
        assert!(!parsed.get_flags().disable_p2p);
        assert_eq!(parsed.get_flags().multi_thread_count, 7);
    }

    #[test]
    fn persisted_field_mappings_cover_the_current_management_schema() {
        use prost_reflect::{DescriptorPool, Value};

        let pool = DescriptorPool::decode(easytier_proto::ALL_DESCRIPTOR_BYTES).unwrap();
        let config = pool
            .get_message_by_name("api.manage.NetworkConfig")
            .unwrap();
        for field in config.fields() {
            let deprecated = matches!(
                field.options().get_field_by_name("deprecated").as_deref(),
                Some(Value::Bool(true))
            );
            if deprecated || field.name() == "instance_id" {
                assert!(
                    document_paths(field.name()).is_none(),
                    "{} must not become editable just because it is in NetworkConfig",
                    field.name()
                );
            } else {
                assert!(
                    document_paths(field.name()).is_some(),
                    "{} needs an explicit persisted TOML mapping",
                    field.name()
                );
            }
        }
    }

    #[test]
    fn deprecated_masks_are_rejected_even_without_values() {
        for field in [
            "enable_vpn_portal",
            "vpn_portal_listen_port",
            "vpn_portal_client_network_addr",
            "vpn_portal_client_network_len",
            "quic_listen_port",
        ] {
            let error =
                merge_persisted_config(ORIGINAL, &NetworkConfig::default(), &[field.into()])
                    .unwrap_err();
            assert_eq!(
                public_patch_error(&error),
                format!("field is not editable (fields: {field})")
            );
        }
    }

    #[test]
    fn patch_diagnostics_name_fields_without_configuration_values() {
        let values = NetworkConfig {
            peer_urls: vec!["private-url-value".into()],
            network_secret: Some("private-network-secret".into()),
            ..Default::default()
        };
        let error = merge_persisted_config(ORIGINAL, &values, &["peer_urls".into()]).unwrap_err();
        assert_eq!(
            public_patch_error(&error),
            "invalid configuration values (fields: peer_urls)"
        );
        assert!(!format!("{error:#?}").contains("private-"));

        let error = merge_persisted_config(
            "network_secret = [ private-persisted-secret",
            &values,
            &["peer_urls".into()],
        )
        .unwrap_err();
        assert_eq!(public_patch_error(&error), "invalid persisted TOML");
        assert!(!format!("{error:#?}").contains("private-"));

        let error = merge_persisted_config(
            ORIGINAL,
            &NetworkConfig::default(),
            &["private-value\ninvalid".into()],
        )
        .unwrap_err();
        assert_eq!(public_patch_error(&error), "invalid field mask path");
        let error = merge_persisted_config(
            ORIGINAL,
            &NetworkConfig::default(),
            &["future_option".into()],
        )
        .unwrap_err();
        assert_eq!(
            public_patch_error(&error),
            "field is not editable (fields: future_option)"
        );
        let error = merge_persisted_config(ORIGINAL, &NetworkConfig::default(), &["a".repeat(129)])
            .unwrap_err();
        assert_eq!(public_patch_error(&error), "field is not editable");
        assert_eq!(
            public_patch_error(&anyhow::anyhow!("unexpected private-secret-context")),
            "invalid field mask or configuration values"
        );
    }

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
            Some("transport:http3-rfc9114-v2")
        );
        assert!(
            unsupported_capability(
                &values,
                &["peer_urls".into()],
                &["transport:http3-rfc9114-v2".into()]
            )
            .is_none()
        );
        assert_eq!(
            unsupported_capability(&values, &["sni".into()], &[]).as_deref(),
            Some("config:sni")
        );
    }

    #[test]
    fn capability_checks_follow_shared_metadata_for_nested_and_reset_values() {
        let explicit_resets = NetworkConfig {
            sni: Some(String::new()),
            p2p_prefer_protocol: Some(String::new()),
            p2p_disguise_mode: Some(String::new()),
            only_use_wss_http3_for_hole_punching: Some(false),
            prefer_wss_http3_for_p2p: Some(false),
            disable_wss_http3_for_p2p: Some(false),
            close_redundant_conns_when_disguised: Some(false),
            ..Default::default()
        };
        let unset = NetworkConfig::default();
        for entry in NetworkConfig::field_capabilities() {
            assert_eq!(
                required_capability(entry.field).as_deref(),
                Some(entry.capability),
            );
            assert_eq!(
                required_capability(&format!("{}.nested", entry.field)).as_deref(),
                Some(entry.capability),
            );
            // A selected false/empty value or omitted value can remove a
            // setting, so neither may bypass the field's capability gate.
            for values in [&explicit_resets, &unset] {
                assert_eq!(
                    unsupported_capability(values, &[entry.field.into()], &[]).as_deref(),
                    Some(entry.capability),
                );
                assert!(
                    unsupported_capability(
                        values,
                        &[entry.field.into()],
                        &[entry.capability.into()],
                    )
                    .is_none(),
                    "{}",
                    entry.field,
                );
            }
        }
        assert_eq!(required_capability("hostname"), None);
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
