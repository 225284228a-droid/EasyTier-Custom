//! Advanced settings are the feature switches and transport parameters a
//! network's members may share without depending on them to connect. A batch
//! edit is incremental: it names the settings the user touched, and every
//! other setting keeps each member's own value. Settings that describe one
//! device (its addresses, routes, exits, proxies) are not advanced settings
//! and are never batch-edited.
//!
//! Each edit stamps the node's `advanced_sync_at` (UTC milliseconds), so a
//! member that missed an edit receives it once when it reconnects unless
//! another console edited later; edits made on the node carry no stamp and
//! are overwritten.
//!
//! The network's access control travels the same way as `network_acl`: its
//! value is the whole policy, or null to remove it, and it only replaces the
//! network's part of each member's `acl`.

use std::collections::BTreeMap;

use easytier::proto::api::manage::NetworkConfig;
use serde_json::Value;

use super::network_acl::{self, NetworkAcl};

/// The change that carries the network's access control.
pub const NETWORK_ACL: &str = "network_acl";

fn network_acl_of(value: &Value) -> anyhow::Result<Option<NetworkAcl>> {
    let acl: Option<NetworkAcl> = serde_json::from_value(value.clone())
        .map_err(|error| anyhow::anyhow!("invalid {NETWORK_ACL}: {error}"))?;
    if let Some(acl) = &acl {
        acl.validate()?;
    }
    Ok(acl.map(NetworkAcl::normalized))
}

/// A setting's value by field name; `null` is the setting left unset, which
/// runs with the core's default.
pub type AdvancedValues = BTreeMap<String, Value>;

/// The settings a user touched; the others keep each member's value.
pub type AdvancedChange = BTreeMap<String, Value>;

macro_rules! advanced_fields {
    (bool: [$($flag:ident),* $(,)?], other: [$($field:ident: $kind:ident),* $(,)?]) => {
        /// Every field a batch edit or a preset may set.
        pub const FIELDS: &[&str] = &[$(stringify!($flag),)* $(stringify!($field),)*];

        fn get(config: &NetworkConfig, field: &str) -> Option<Value> {
            match field {
                $(stringify!($flag) => Some(config.$flag.map(Value::Bool).unwrap_or(Value::Null)),)*
                $(stringify!($field) => Some(advanced_fields!(@get $kind, config.$field)),)*
                _ => None,
            }
        }

        fn set(config: &mut NetworkConfig, field: &str, value: &Value) -> anyhow::Result<()> {
            match field {
                $(stringify!($flag) => config.$flag = match value {
                    Value::Null => None,
                    Value::Bool(value) => Some(*value),
                    _ => anyhow::bail!("{field} must be true, false or null"),
                },)*
                $(stringify!($field) => config.$field = advanced_fields!(@set $kind, field, value),)*
                _ => anyhow::bail!("{field} is not an advanced setting"),
            }
            Ok(())
        }
    };
    (@get int, $value:expr) => { $value.map(|value| Value::from(value as i64)).unwrap_or(Value::Null) };
    (@get text, $value:expr) => { $value.clone().map(Value::String).unwrap_or(Value::Null) };
    (@set int, $field:ident, $value:ident) => {
        match $value {
            Value::Null => None,
            Value::Number(number) => Some(
                number
                    .as_i64()
                    .and_then(|number| number.try_into().ok())
                    .ok_or_else(|| anyhow::anyhow!("{} is out of range", $field))?,
            ),
            _ => anyhow::bail!("{} must be a number or null", $field),
        }
    };
    (@set text, $field:ident, $value:ident) => {
        match $value {
            Value::Null => None,
            Value::String(text) => Some(text.clone()),
            _ => anyhow::bail!("{} must be text or null", $field),
        }
    };
}

// Left out on purpose: exit forwarding, TUN, smoltcp, device binding and
// system forwarding describe one device; encryption is a core setting.
advanced_fields! {
    bool: [
        latency_first,
        disable_ipv6,
        enable_kcp_proxy,
        disable_kcp_input,
        disable_p2p,
        relay_all_peer_rpc,
        multi_thread,
        disable_udp_hole_punching,
        enable_magic_dns,
        enable_private_mode,
        enable_quic_proxy,
        disable_quic_input,
        disable_sym_hole_punching,
        p2p_only,
        disable_tcp_hole_punching,
        lazy_p2p,
        need_p2p,
        disable_upnp,
        disable_relay_data,
        enable_udp_broadcast_relay,
        prefer_peer_relay,
        close_redundant_conns_when_disguised,
        disable_parallel_connections,
        enable_udp_fec,
        enable_udp_fec_kcp,
        enable_udp_fec_quic,
    ],
    other: [
        data_compress_algo: int,
        mtu: int,
        traffic_disguise: text,
        parallel_connections: int,
        http3_fec_ratio: int,
        http3_streams: int,
        p2p_prefer_protocol: text,
        p2p_disguise_mode: text,
    ]
}

/// The values a text setting may take; the core rejects anything else.
fn choices(field: &str) -> Option<&'static [&'static str]> {
    match field {
        "p2p_prefer_protocol" => Some(&["tcp", "udp"]),
        "p2p_disguise_mode" => Some(&["prefer", "off", "disabled", "only"]),
        _ => None,
    }
}

/// Every advanced setting `config` holds.
pub fn values_of(config: &NetworkConfig) -> AdvancedValues {
    FIELDS
        .iter()
        .map(|field| {
            (
                (*field).to_owned(),
                get(config, field).unwrap_or(Value::Null),
            )
        })
        .collect()
}

pub fn validate(change: &AdvancedChange) -> anyhow::Result<()> {
    let mut probe = NetworkConfig::default();
    for (field, value) in change {
        if field == NETWORK_ACL {
            network_acl_of(value)?;
            continue;
        }
        set(&mut probe, field, value)?;
        if field == "http3_fec_ratio" {
            anyhow::ensure!(
                probe.http3_fec_ratio.is_none_or(|ratio| ratio <= 100),
                "http3_fec_ratio must be between 0 and 100"
            );
        }
        if field == "http3_streams" {
            anyhow::ensure!(
                probe
                    .http3_streams
                    .is_none_or(|streams| (1..=32).contains(&streams)),
                "http3_streams must be between 1 and 32"
            );
        }
        if let (Some(choices), Some(text)) = (choices(field), value.as_str()) {
            anyhow::ensure!(
                choices.contains(&text),
                "{field} must be one of {}",
                choices.join(", ")
            );
        }
    }
    Ok(())
}

pub const SYNC_CAPABILITY: &str = "config:advanced-sync-v1";

/// The write that gives `config` the changed settings it supports, if any
/// differ, stamped with `stamp` when the node keeps it. `supports` says
/// whether the node can store a field that needs a capability; fields it
/// cannot store are left out.
pub fn patch(
    config: &NetworkConfig,
    change: &AdvancedChange,
    supports: impl Fn(&str) -> bool,
    stamp: Option<u64>,
) -> anyhow::Result<Option<(NetworkConfig, Vec<String>)>> {
    let mut desired = config.clone();
    let mut mask = Vec::new();
    for (field, value) in change {
        if field == NETWORK_ACL {
            network_acl::apply(&mut desired, network_acl_of(value)?.as_ref());
            if desired.acl != config.acl {
                mask.push("acl".to_owned());
            }
            continue;
        }
        if NetworkConfig::required_capability(field).is_some_and(|capability| !supports(capability))
        {
            continue;
        }
        if get(config, field).as_ref() == Some(value) {
            continue;
        }
        set(&mut desired, field, value)?;
        mask.push(field.clone());
    }
    if mask.is_empty() {
        return Ok(None);
    }
    if let Some(stamp) = stamp.filter(|_| supports(SYNC_CAPABILITY)) {
        desired.advanced_sync_at = Some(stamp);
        mask.push("advanced_sync_at".to_owned());
    }
    Ok(Some((desired, mask)))
}

/// Several missed edits replay once as one: the later wins field by field.
pub fn then(earlier: &AdvancedChange, later: &AdvancedChange) -> AdvancedChange {
    let mut combined = earlier.clone();
    combined.extend(
        later
            .iter()
            .map(|(field, value)| (field.clone(), value.clone())),
    );
    combined
}

/// Apply `values` to a new configuration, leaving out fields the node cannot
/// store.
pub fn apply_to(
    config: &mut NetworkConfig,
    values: &AdvancedValues,
    supports: impl Fn(&str) -> bool,
) -> anyhow::Result<Vec<String>> {
    let mut written = Vec::new();
    for (field, value) in values {
        if value.is_null()
            || NetworkConfig::required_capability(field)
                .is_some_and(|capability| !supports(capability))
        {
            continue;
        }
        set(config, field, value)?;
        written.push(field.clone());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn change(entries: &[(&str, Value)]) -> AdvancedChange {
        entries
            .iter()
            .map(|(field, value)| ((*field).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn writes_only_touched_settings_that_differ() {
        let config = NetworkConfig {
            latency_first: Some(true),
            enable_kcp_proxy: Some(false),
            mtu: Some(1300),
            hostname: Some("node".into()),
            ..Default::default()
        };
        let edit = change(&[
            ("latency_first", json!(true)),
            ("enable_kcp_proxy", json!(true)),
            ("mtu", Value::Null),
        ]);
        let (desired, mask) = patch(&config, &edit, |_| true, Some(9)).unwrap().unwrap();
        assert_eq!(mask, ["enable_kcp_proxy", "mtu", "advanced_sync_at"]);
        assert_eq!(desired.advanced_sync_at, Some(9));
        assert_eq!(desired.enable_kcp_proxy, Some(true));
        assert_eq!(desired.mtu, None);
        assert_eq!(desired.latency_first, Some(true));
        assert_eq!(desired.hostname.as_deref(), Some("node"));
        assert!(
            patch(&desired, &edit, |_| true, Some(10))
                .unwrap()
                .is_none(),
            "a second write changes nothing"
        );
    }

    #[test]
    fn leaves_out_settings_the_node_cannot_store() {
        let edit = change(&[
            ("traffic_disguise", json!("video")),
            ("disable_p2p", json!(true)),
        ]);
        let (desired, mask) = patch(&NetworkConfig::default(), &edit, |_| false, Some(9))
            .unwrap()
            .unwrap();
        assert_eq!(mask, ["disable_p2p"]);
        assert_eq!(
            desired.advanced_sync_at, None,
            "official cores keep no stamp"
        );
    }

    #[test]
    fn rejects_device_settings_and_wrong_types() {
        assert!(validate(&change(&[("enable_exit_node", json!(true))])).is_err());
        assert!(validate(&change(&[("use_smoltcp", json!(true))])).is_err());
        assert!(validate(&change(&[("enable_private_mode", json!(true))])).is_ok());
        assert!(validate(&change(&[("latency_first", json!(1))])).is_err());
        assert!(validate(&change(&[("mtu", json!("1300"))])).is_err());
        assert!(validate(&change(&[("mtu", json!(1300)), ("lazy_p2p", Value::Null)])).is_ok());
    }

    #[test]
    fn p2p_protocol_and_disguise_take_only_what_the_core_knows() {
        assert!(validate(&change(&[("p2p_prefer_protocol", json!("tcp"))])).is_ok());
        assert!(validate(&change(&[("p2p_prefer_protocol", json!("http3"))])).is_err());
        assert!(validate(&change(&[("p2p_disguise_mode", json!("only"))])).is_ok());
        assert!(validate(&change(&[("p2p_disguise_mode", json!("always"))])).is_err());
        assert!(validate(&change(&[("p2p_disguise_mode", Value::Null)])).is_ok());

        // A node without the capability keeps its own value.
        let edit = change(&[
            ("p2p_disguise_mode", json!("off")),
            ("p2p_prefer_protocol", json!("tcp")),
        ]);
        assert!(
            patch(&NetworkConfig::default(), &edit, |_| false, None)
                .unwrap()
                .is_none()
        );
        let (desired, mask) = patch(&NetworkConfig::default(), &edit, |_| true, None)
            .unwrap()
            .unwrap();
        assert_eq!(mask, ["p2p_disguise_mode", "p2p_prefer_protocol"]);
        assert_eq!(desired.p2p_disguise_mode.as_deref(), Some("off"));
    }

    #[test]
    fn network_acl_writes_only_the_acl_and_only_when_it_differs() {
        let config = NetworkConfig {
            instance_id: Some("member".into()),
            mtu: Some(1300),
            ..Default::default()
        };
        let acl = NetworkAcl {
            default_action: crate::central_network::model::AclAction::Deny,
            ..Default::default()
        };
        let edit = change(&[(NETWORK_ACL, serde_json::to_value(Some(&acl)).unwrap())]);
        validate(&edit).unwrap();
        let (desired, mask) = patch(&config, &edit, |_| true, Some(9)).unwrap().unwrap();
        assert_eq!(mask, ["acl", "advanced_sync_at"]);
        assert_eq!(desired.mtu, Some(1300));
        assert!(network_acl::read(desired.acl.as_ref()).network.is_some());
        assert!(
            patch(&desired, &edit, |_| true, Some(10))
                .unwrap()
                .is_none()
        );

        let removal = change(&[(NETWORK_ACL, Value::Null)]);
        let (removed, _) = patch(&desired, &removal, |_| true, Some(11))
            .unwrap()
            .unwrap();
        assert_eq!(removed.acl, None);
        assert!(validate(&change(&[(NETWORK_ACL, json!({"rules": [{"id": "x"}]}))])).is_err());
    }

    #[test]
    fn later_missed_edits_win_field_by_field() {
        let first = change(&[("mtu", json!(1200)), ("lazy_p2p", json!(true))]);
        let second = change(&[("mtu", json!(1100))]);
        assert_eq!(
            then(&first, &second),
            change(&[("lazy_p2p", json!(true)), ("mtu", json!(1100))])
        );
    }
}
