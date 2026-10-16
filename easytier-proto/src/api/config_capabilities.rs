use super::manage::NetworkConfig;

/// The concrete runtime facilities needed to expose an extension field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFieldSupport {
    Always,
    TcpAndUdp,
    TlsTransport,
    DisguisedP2p,
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigFieldCapability {
    pub field: &'static str,
    pub capability: &'static str,
    pub support: ConfigFieldSupport,
}

impl ConfigFieldCapability {
    pub fn supported_by(&self, protocols: &[String], disguised_p2p: bool) -> bool {
        let supports = |name: &str| protocols.iter().any(|protocol| protocol == name);
        let tls = supports("wss") || supports("http3");
        match self.support {
            ConfigFieldSupport::Always => true,
            ConfigFieldSupport::TcpAndUdp => supports("tcp") && supports("udp"),
            ConfigFieldSupport::TlsTransport => tls,
            ConfigFieldSupport::DisguisedP2p => disguised_p2p && tls,
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/config_capabilities.rs"));

impl NetworkConfig {
    /// Extension fields and their gates, also consumed by frontend codegen.
    pub fn field_capabilities() -> &'static [ConfigFieldCapability] {
        CONFIG_FIELD_CAPABILITIES
    }

    pub fn required_capability(path: &str) -> Option<&'static str> {
        let field = path.split('.').next()?;
        Self::field_capabilities()
            .iter()
            .find(|entry| entry.field == field)
            .map(|entry| entry.capability)
    }
}
