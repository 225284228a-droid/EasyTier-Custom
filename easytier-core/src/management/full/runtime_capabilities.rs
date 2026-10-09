//! Advertise implemented configuration fields from the concrete host policy,
//! not from a local-configuration flag or a version-string guess.

use crate::instance::CoreInstanceHostConfig;

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
        capabilities.push("transport:http3-framed-v1".into());
    }
    if supports("tcp") && supports("udp") {
        capabilities.push("config:p2p_prefer_protocol".into());
    }
    if supports("wss") || supports("http3") {
        capabilities.push("config:sni".into());
    }
    if host.wss_http3_supported && (supports("wss") || supports("http3")) {
        for field in [
            "only_use_wss_http3_for_hole_punching",
            "prefer_wss_http3_for_p2p",
            "disable_wss_http3_for_p2p",
            "close_redundant_conns_when_disguised",
        ] {
            capabilities.push(format!("config:{field}"));
        }
    }
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
            vec!["config:p2p_prefer_protocol"]
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
        assert!(capabilities.contains(&"transport:http3-framed-v1".into()));
        assert!(!capabilities.contains(&"transport:wss".into()));
        assert!(capabilities.contains(&"config:sni".into()));
    }
}
