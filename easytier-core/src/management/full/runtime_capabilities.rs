//! Advertise implemented configuration fields from the concrete host policy,
//! not from a local-configuration flag or a version-string guess.

use crate::instance::CoreInstanceHostConfig;
use easytier_proto::api::manage::NetworkConfig;

pub fn management_capabilities_for_host(host: &CoreInstanceHostConfig) -> Vec<String> {
    let supports = |name: &str| {
        host.endpoint_protocols
            .iter()
            .any(|protocol| protocol == name)
    };
    let mut capabilities = Vec::new();
    for protocol in ["ws", "wss"] {
        if supports(protocol) {
            capabilities.push(format!("transport:{protocol}"));
        }
    }
    if supports("http3") {
        capabilities.push("transport:http3-rfc9114-v2".into());
    }
    capabilities.extend(
        NetworkConfig::field_capabilities()
            .iter()
            .filter(|entry| entry.supported_by(&host.endpoint_protocols, host.wss_http3_supported))
            .map(|entry| entry.capability.to_owned()),
    );
    capabilities.push("config:private-mode-default-true-v1".into());
    capabilities.sort();
    capabilities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_host_does_not_inherit_disguised_or_quic_build_capabilities() {
        let host = CoreInstanceHostConfig {
            endpoint_protocols: vec!["tcp".into(), "udp".into()],
            quic_enabled: false,
            wss_http3_supported: false,
            ..Default::default()
        };
        assert_eq!(
            management_capabilities_for_host(&host),
            vec![
                "config:p2p_prefer_protocol",
                "config:private-mode-default-true-v1",
                "config:vpn-subnet-exit-v1"
            ]
        );
    }

    #[test]
    fn registered_http3_and_wss_are_advertised_independently() {
        let host = CoreInstanceHostConfig {
            endpoint_protocols: vec!["tcp".into(), "udp".into(), "http3".into()],
            quic_enabled: true,
            wss_http3_supported: true,
            ..Default::default()
        };
        let capabilities = management_capabilities_for_host(&host);
        assert!(capabilities.contains(&"transport:http3-rfc9114-v2".into()));
        assert!(!capabilities.contains(&"transport:wss".into()));
        assert!(capabilities.contains(&"config:sni".into()));
    }

    #[test]
    fn extension_gates_follow_registered_transports_and_p2p_support() {
        let cases: &[(&[&str], bool, bool, bool, bool)] = &[
            (&[], false, false, false, false),
            (&["tcp"], true, false, false, false),
            (&["tcp", "udp"], false, true, false, false),
            (&["ws"], true, false, false, false),
            (&["wss"], false, false, true, false),
            (&["wss"], true, false, true, true),
            (&["http3"], true, false, true, true),
            (&["tcp", "udp", "wss", "http3"], true, true, true, true),
        ];
        for &(protocols, disguised_p2p, prefer_protocol, sni, mode) in cases {
            let host = CoreInstanceHostConfig {
                endpoint_protocols: protocols
                    .iter()
                    .map(|protocol| (*protocol).to_owned())
                    .collect(),
                wss_http3_supported: disguised_p2p,
                ..Default::default()
            };
            let capabilities = management_capabilities_for_host(&host);
            for (field, expected) in [
                ("p2p_prefer_protocol", prefer_protocol),
                ("sni", sni),
                ("p2p_disguise_mode", mode),
                ("prefer_wss_http3_for_p2p", mode),
                ("disable_wss_http3_for_p2p", mode),
                ("only_use_wss_http3_for_hole_punching", mode),
                ("close_redundant_conns_when_disguised", mode),
                ("vpn", true),
            ] {
                let required = NetworkConfig::required_capability(field).unwrap();
                assert_eq!(
                    capabilities.iter().any(|capability| capability == required),
                    expected,
                    "field={field}, protocols={protocols:?}, disguised_p2p={disguised_p2p}"
                );
                assert_eq!(
                    NetworkConfig::required_capability(&format!("{field}.nested")),
                    Some(required)
                );
            }
            assert_eq!(NetworkConfig::required_capability("hostname"), None);
        }
    }
}
