//! Versioned, serialized inputs accepted by the WASI instance lifecycle ABI.

use serde::{Deserialize, Serialize};

use crate::{
    config::toml::TomlConfig, connectivity::connector_host::HostConnectorEnvironmentSnapshot,
};

pub(crate) const WASI_CORE_INSTANCE_CONFIG_VERSION: u32 =
    crate::wasi::abi::CORE_INSTANCE_CONFIG_VERSION;
#[cfg(all(target_os = "wasi", feature = "management"))]
pub(crate) const WASI_WEB_CLIENT_CONFIG_VERSION: u32 = crate::wasi::abi::WEB_CLIENT_CONFIG_VERSION;

/// Versioned payload accepted by host-driven instance frontends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WasiCoreInstanceCreateConfig {
    pub version: u32,
    pub config: String,
    pub environment: HostConnectorEnvironmentSnapshot,
}

impl WasiCoreInstanceCreateConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.version != WASI_CORE_INSTANCE_CONFIG_VERSION {
            anyhow::bail!(
                "unsupported host core instance config version: {}",
                self.version
            );
        }
        Ok(())
    }

    pub fn parse_config(&self) -> anyhow::Result<TomlConfig> {
        // The lifecycle ABI also handles explicit API creates and management
        // replacements. Recovering invalid input here would report success for
        // a different configuration than the caller requested.
        TomlConfig::new_from_str_with_source("WASI create config", &self.config)
    }
}

#[cfg(all(target_os = "wasi", feature = "management"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WasiWebClientCreateConfig {
    pub version: u32,
    pub endpoint: String,
    pub machine_id: String,
    pub hostname: String,
    pub secure_mode: bool,
    pub os_type: String,
    pub environment: HostConnectorEnvironmentSnapshot,
}

#[cfg(all(target_os = "wasi", feature = "management"))]
impl WasiWebClientCreateConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.version != WASI_WEB_CLIENT_CONFIG_VERSION {
            anyhow::bail!(
                "unsupported host WebClient config version: {}",
                self.version
            );
        }
        uuid::Uuid::parse_str(&self.machine_id)?;
        Ok(())
    }
}

#[cfg(feature = "wasm-host-tunnel")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WasiHostTunnelMetadata {
    pub version: u32,
    pub local_url: url::Url,
    pub remote_url: url::Url,
    #[serde(default)]
    pub resolved_remote_url: Option<url::Url>,
}

#[cfg(feature = "wasm-host-tunnel")]
impl WasiHostTunnelMetadata {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        if self.version != crate::wasi::abi::HOST_TUNNEL_ABI_VERSION {
            anyhow::bail!("unsupported host tunnel ABI version: {}", self.version);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::toml::ConfigLoader as _;

    fn create_config(config: &str) -> WasiCoreInstanceCreateConfig {
        let create = WasiCoreInstanceCreateConfig {
            version: WASI_CORE_INSTANCE_CONFIG_VERSION,
            config: config.to_owned(),
            environment: HostConnectorEnvironmentSnapshot::default(),
        };
        create.validate().unwrap();
        create
    }

    #[test]
    fn host_create_rejects_invalid_configuration_instead_of_recovering() {
        for (input, expected) in [
            ("[", "WASI create config"),
            ("dhcp = 'yes'", "invalid DHCP CIDR"),
            ("[vpn]\ndns_protect = true", "dns_protect"),
            (
                "[[vpn.rules]]\ncidr = '203.0.113.0/24'\naction = 'exit'\nexits = ['10.144.0.2']",
                "unknown field",
            ),
            (
                "[[vpn.rules]]\ncidr = '203.0.113.0/24'\nexit = '10.144.0.2'\n[[vpn.rules]]\ncidr = '203.0.113.0/24'\nexit = '10.144.0.3'",
                "duplicate VPN rule CIDR",
            ),
            (
                "[[vpn.rules]]\ncidr = '203.0.113.0/24'\nexit = '127.0.0.1'",
                "invalid exit address",
            ),
        ] {
            let error = create_config(input).parse_config().unwrap_err();
            assert!(error.to_string().contains(expected), "{input}: {error}");
        }
    }

    #[test]
    fn host_create_preserves_valid_settings_for_host_capability_normalization() {
        let config = create_config(
            r#"
listeners = ["tcp://0.0.0.0:11010", "wg://0.0.0.0:11011"]
[[peer]]
uri = "tcp://127.0.0.1:11010"
[[peer]]
uri = "wg://127.0.0.1:11011"
[flags]
enable_kcp_proxy = true
enable_quic_proxy = true
"#,
        )
        .parse_config()
        .unwrap();
        config.get_id();
        let before = config.dump();
        let normalized = crate::instance::CoreInstanceConfig::from_toml_with_host(
            &config,
            &crate::instance::CoreInstanceHostConfig {
                ignore_unsupported_config: true,
                kcp_enabled: false,
                quic_enabled: false,
                endpoint_protocols: vec!["tcp".to_owned(), "udp".to_owned()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(config.dump(), before);
        assert_eq!(config.get_peers().len(), 2);
        assert!(config.get_flags().enable_kcp_proxy);
        assert!(config.get_flags().enable_quic_proxy);
        assert_eq!(normalized.connectivity.initial_peers.len(), 1);
        assert_eq!(
            normalized
                .connectivity
                .listeners
                .as_ref()
                .unwrap()
                .urls
                .len(),
            1
        );
        assert!(!normalized.peer.snapshot.flags.enable_kcp_proxy);
        assert!(!normalized.peer.snapshot.flags.enable_quic_proxy);
    }

    #[cfg(feature = "wasm-host-tunnel")]
    #[test]
    fn host_tunnel_metadata_accepts_transport_neutral_urls() {
        let metadata = WasiHostTunnelMetadata {
            version: crate::wasi::abi::HOST_TUNNEL_ABI_VERSION,
            local_url: "test-tunnel://listener.example/".parse().unwrap(),
            remote_url: "test-tunnel://peer.example/".parse().unwrap(),
            resolved_remote_url: None,
        };

        metadata.validate().unwrap();
    }
}
