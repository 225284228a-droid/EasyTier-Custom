//! Complete EasyTier TOML configuration model.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use super::{DhcpConfig, normalize_secure_mode_config};
pub use super::{EncryptionAlgorithm, gateway::PortForwardConfig};
use anyhow::Context;
#[cfg(feature = "rich-config-errors")]
use ariadne::{CharSet, Config as AriadneConfig, IndexType, Label, Report, ReportKind, Source};
use optionize::Optionizable;
use serde::{Deserialize, Serialize};

use crate::proto::{acl::Acl, common::SecureModeConfig};

pub use crate::proto::common::{Flags, FlagsPatch};

mod tolerant;

pub(crate) fn default_instance_name() -> String {
    "default".to_owned()
}

pub(crate) fn validate_transport_tuning(
    count: Option<u32>,
    style: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        count.is_none_or(|count| (1..=16).contains(&count)),
        "parallel_connections must be an integer between 1 and 16"
    );
    anyhow::ensure!(
        style.is_none_or(|style| matches!(style, "speedtest" | "video")),
        "traffic_disguise must be speedtest or video"
    );
    Ok(())
}

#[auto_impl::auto_impl(Box, &)]
pub trait ConfigLoader: Send + Sync {
    fn get_id(&self) -> uuid::Uuid;
    fn set_id(&self, id: uuid::Uuid);

    fn get_hostname(&self) -> String;
    fn set_hostname(&self, name: Option<String>);

    fn get_sni(&self) -> String;
    fn set_sni(&self, sni: Option<String>);

    fn get_parallel_connections(&self) -> u32 {
        1
    }
    fn set_parallel_connections(&self, _count: Option<u32>) {}
    fn get_disable_parallel_connections(&self) -> bool {
        false
    }
    fn set_disable_parallel_connections(&self, _disabled: Option<bool>) {}
    fn get_traffic_disguise(&self) -> String {
        "speedtest".to_owned()
    }
    fn set_traffic_disguise(&self, _style: Option<String>) {}

    fn get_inst_name(&self) -> String;
    fn set_inst_name(&self, name: String);

    fn get_netns(&self) -> Option<String>;
    fn set_netns(&self, ns: Option<String>);

    fn get_ipv4(&self) -> Option<cidr::Ipv4Inet>;
    fn set_ipv4(&self, addr: Option<cidr::Ipv4Inet>);

    fn get_ipv6(&self) -> Option<cidr::Ipv6Inet>;
    fn set_ipv6(&self, addr: Option<cidr::Ipv6Inet>);

    fn get_dhcp_config(&self) -> DhcpConfig;
    fn set_dhcp_config(&self, dhcp: DhcpConfig);

    fn get_dhcp(&self) -> bool {
        self.get_dhcp_config().enabled()
    }

    fn set_dhcp(&self, dhcp: bool) {
        self.set_dhcp_config(DhcpConfig::Enabled(dhcp));
    }

    fn get_dhcpv6_config(&self) -> DhcpConfig<cidr::Ipv6Cidr>;
    fn set_dhcpv6_config(&self, dhcp: DhcpConfig<cidr::Ipv6Cidr>);

    fn get_dhcpv6(&self) -> bool {
        self.get_dhcpv6_config().enabled()
    }

    fn set_dhcpv6(&self, dhcp: bool) {
        self.set_dhcpv6_config(DhcpConfig::Enabled(dhcp));
    }

    fn add_proxy_cidr(
        &self,
        cidr: cidr::Ipv4Cidr,
        mapped_cidr: Option<cidr::Ipv4Cidr>,
    ) -> Result<(), anyhow::Error>;
    fn remove_proxy_cidr(&self, cidr: cidr::Ipv4Cidr);
    fn clear_proxy_cidrs(&self);
    fn get_proxy_cidrs(&self) -> Vec<ProxyNetworkConfig>;

    fn get_network_identity(&self) -> NetworkIdentity;
    fn set_network_identity(&self, identity: NetworkIdentity);

    fn get_listener_uris(&self) -> Vec<url::Url>;

    fn get_peers(&self) -> Vec<PeerConfig>;
    fn set_peers(&self, peers: Vec<PeerConfig>);

    fn get_listeners(&self) -> Option<Vec<url::Url>>;
    fn set_listeners(&self, listeners: Vec<url::Url>);

    fn get_mapped_listeners(&self) -> Vec<url::Url>;
    fn set_mapped_listeners(&self, listeners: Option<Vec<url::Url>>);

    fn get_vpn_portal_config(&self) -> Option<VpnPortalConfig>;
    fn set_vpn_portal_config(&self, config: VpnPortalConfig);

    fn get_flags(&self) -> Flags;
    /// Replace all flags with explicitly supplied values.
    fn set_flags(&self, flags: Flags);
    /// The user-supplied values, before application defaults are resolved.
    fn get_flags_patch(&self) -> FlagsPatch;
    /// Apply only supplied fields, preserving absence and explicit defaults.
    fn patch_flags(&self, flags: FlagsPatch);

    fn get_vpn(&self) -> crate::config::vpn::VpnConfig;
    fn set_vpn(&self, vpn: crate::config::vpn::VpnConfig);

    fn get_routes(&self) -> Option<Vec<cidr::Ipv4Cidr>>;
    fn set_routes(&self, routes: Option<Vec<cidr::Ipv4Cidr>>);

    fn get_socks5_portal(&self) -> Option<url::Url>;
    fn set_socks5_portal(&self, addr: Option<url::Url>);

    fn get_port_forwards(&self) -> Vec<PortForwardConfig>;
    fn set_port_forwards(&self, forwards: Vec<PortForwardConfig>);

    fn get_acl(&self) -> Option<Acl>;
    fn set_acl(&self, acl: Option<Acl>);

    fn get_tcp_whitelist(&self) -> Vec<String>;
    fn set_tcp_whitelist(&self, whitelist: Vec<String>);

    fn get_udp_whitelist(&self) -> Vec<String>;
    fn set_udp_whitelist(&self, whitelist: Vec<String>);

    fn get_stun_servers(&self) -> Option<Vec<String>>;
    fn set_stun_servers(&self, servers: Option<Vec<String>>);

    fn get_tcp_stun_servers(&self) -> Option<Vec<String>> {
        None
    }
    fn set_tcp_stun_servers(&self, _servers: Option<Vec<String>>) {}

    fn get_stun_servers_v6(&self) -> Option<Vec<String>>;
    fn set_stun_servers_v6(&self, servers: Option<Vec<String>>);

    fn get_secure_mode(&self) -> Option<SecureModeConfig>;
    fn set_secure_mode(&self, secure_mode: Option<SecureModeConfig>);

    fn get_credential_file(&self) -> Option<std::path::PathBuf> {
        None
    }
    fn set_credential_file(&self, _path: Option<std::path::PathBuf>) {}

    fn get_managed_credentials(&self) -> Vec<ManagedCredentialConfig> {
        Vec::new()
    }
    fn set_managed_credentials(&self, _credentials: Vec<ManagedCredentialConfig>) {}

    fn get_network_config_source(&self) -> ConfigSource {
        ConfigSource::User
    }
    fn set_network_config_source(&self, _source: Option<ConfigSource>) {}

    fn dump(&self) -> String;
    fn dump_redacted(&self) -> String {
        self.dump()
    }
}

pub trait LoggingConfigLoader {
    fn get_file_logger_config(&self) -> FileLoggerConfig;

    fn get_console_logger_config(&self) -> ConsoleLoggerConfig;
}

use super::NetworkSecretDigest;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkIdentity {
    pub network_name: String,
    pub network_secret: Option<String>,
    #[serde(skip)]
    pub network_secret_digest: Option<NetworkSecretDigest>,
}

impl From<super::NetworkIdentity> for NetworkIdentity {
    fn from(value: super::NetworkIdentity) -> Self {
        Self {
            network_name: value.network_name,
            network_secret: value.network_secret,
            network_secret_digest: value.network_secret_digest,
        }
    }
}

impl From<&NetworkIdentity> for super::NetworkIdentity {
    fn from(value: &NetworkIdentity) -> Self {
        Self {
            network_name: value.network_name.clone(),
            network_secret: value.network_secret.clone(),
            network_secret_digest: value.network_secret_digest,
        }
    }
}

impl From<NetworkIdentity> for super::NetworkIdentity {
    fn from(value: NetworkIdentity) -> Self {
        Self {
            network_name: value.network_name,
            network_secret: value.network_secret,
            network_secret_digest: value.network_secret_digest,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    #[default]
    User,
    Web,
}

impl ConfigSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Web => "web",
        }
    }
}

impl std::str::FromStr for ConfigSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Self::User),
            "web" => Ok(Self::Web),
            other => Err(format!("unknown network config source: {other}")),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
struct ConfigSourceConfig {
    source: ConfigSource,
}

impl PartialEq for NetworkIdentity {
    fn eq(&self, other: &Self) -> bool {
        super::NetworkIdentity::from(self) == super::NetworkIdentity::from(other)
    }
}

impl Eq for NetworkIdentity {}

impl std::hash::Hash for NetworkIdentity {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&super::NetworkIdentity::from(self), state);
    }
}

impl NetworkIdentity {
    pub fn new(network_name: String, network_secret: String) -> Self {
        super::NetworkIdentity::new(network_name, network_secret).into()
    }

    /// Create a NetworkIdentity for a credential node (no network_secret).
    /// The node identifies by network_name only and authenticates via credential keypair.
    pub fn new_credential(network_name: String) -> Self {
        super::NetworkIdentity::new_credential(network_name).into()
    }
}

impl Default for NetworkIdentity {
    fn default() -> Self {
        super::NetworkIdentity::default().into()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PeerConfig {
    pub uri: url::Url,
    pub peer_public_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ProxyNetworkConfig {
    pub cidr: cidr::Ipv4Cidr,                // the CIDR of the proxy network
    pub mapped_cidr: Option<cidr::Ipv4Cidr>, // allow remap the proxy CIDR to another CIDR
    pub allow: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct FileLoggerConfig {
    pub level: Option<String>,
    pub file: Option<String>,
    pub dir: Option<String>,
    pub size_mb: Option<u64>,
    pub count: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct ConsoleLoggerConfig {
    pub level: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, bon::Builder)]
pub struct LoggingConfig {
    #[builder(into)]
    pub file_logger: Option<FileLoggerConfig>,
    #[builder(into)]
    pub console_logger: Option<ConsoleLoggerConfig>,
}

impl LoggingConfigLoader for &LoggingConfig {
    fn get_file_logger_config(&self) -> FileLoggerConfig {
        self.file_logger.clone().unwrap_or_default()
    }

    fn get_console_logger_config(&self) -> ConsoleLoggerConfig {
        self.console_logger.clone().unwrap_or_default()
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VpnPortalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub wireguard_listen: SocketAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wireguard_private_key: Option<String>,
    #[serde(default)]
    pub clients: Vec<VpnPortalClientConfig>,
}

impl std::fmt::Debug for VpnPortalConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VpnPortalConfig")
            .field("enabled", &self.enabled)
            .field("wireguard_listen", &self.wireguard_listen)
            .field(
                "wireguard_private_key",
                &self.wireguard_private_key.as_ref().map(|_| "<redacted>"),
            )
            .field("clients", &self.clients)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VpnPortalClientConfig {
    pub name: String,
    pub virtual_ip: cidr::Ipv4Inet,
    #[serde(default)]
    pub groups: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[optionize::optionized]
#[cfg_attr(any(feature = "web-client", feature = "browser-config"), optionize(object = easytier_proto::api::manage::ManagedCredentialConfig))]
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedCredentialConfig {
    #[optionize(flatten)]
    pub credential_id: String,
    #[optionize(flatten)]
    pub credential_secret: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[optionize(flatten)]
    pub groups: Vec<String>,
    #[serde(default)]
    #[optionize(flatten)]
    pub allow_relay: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[optionize(flatten)]
    pub allowed_proxy_cidrs: Vec<String>,
    #[optionize(flatten)]
    pub expiry_unix: i64,
    #[serde(default = "default_true")]
    // The mapped field is `Option<bool>` (the protocol spells it `optional bool`),
    // so this document default does not describe it.
    #[optionize(attrs(.., -serde), default = |_| default_true())]
    pub reusable: bool,
}

impl std::fmt::Debug for ManagedCredentialConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedCredentialConfig")
            .field("credential_id", &self.credential_id)
            .field("credential_secret", &"<redacted>")
            .field("groups", &self.groups)
            .field("allow_relay", &self.allow_relay)
            .field("allowed_proxy_cidrs", &self.allowed_proxy_cidrs)
            .field("expiry_unix", &self.expiry_unix)
            .field("reusable", &self.reusable)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "config-write", derive(Serialize))]
struct Config {
    netns: Option<String>,
    hostname: Option<String>,
    sni: Option<String>,
    parallel_connections: Option<u32>,
    disable_parallel_connections: Option<bool>,
    traffic_disguise: Option<String>,
    instance_name: Option<String>,
    instance_id: Option<uuid::Uuid>,
    ipv4: Option<String>,
    ipv6: Option<String>,
    dhcp: Option<DhcpConfig>,
    dhcpv6: Option<DhcpConfig<cidr::Ipv6Cidr>>,
    network_identity: Option<NetworkIdentity>,
    listeners: Option<Vec<url::Url>>,
    mapped_listeners: Option<Vec<url::Url>>,
    vpn: Option<crate::config::vpn::VpnConfig>,

    peer: Option<Vec<PeerConfig>>,
    proxy_network: Option<Vec<ProxyNetworkConfig>>,

    vpn_portal_config: Option<VpnPortalConfig>,

    routes: Option<Vec<cidr::Ipv4Cidr>>,

    socks5_proxy: Option<url::Url>,

    port_forward: Option<Vec<PortForwardConfig>>,

    secure_mode: Option<SecureModeConfig>,

    #[serde(default, skip_serializing_if = "flags_patch_is_empty")]
    flags: FlagsPatch,

    acl: Option<Acl>,

    tcp_whitelist: Option<Vec<String>>,
    udp_whitelist: Option<Vec<String>>,
    stun_servers: Option<Vec<String>>,
    tcp_stun_servers: Option<Vec<String>>,
    stun_servers_v6: Option<Vec<String>>,

    credential_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    managed_credentials: Vec<ManagedCredentialConfig>,
    source: Option<ConfigSourceConfig>,
}

#[cfg(feature = "config-write")]
fn flags_patch_is_empty(flags: &FlagsPatch) -> bool {
    prost::Message::encoded_len(flags) == 0
}

#[cfg(feature = "rich-config-errors")]
fn format_toml_parse_error(source_name: &str, config_str: &str, error: &toml::de::Error) -> String {
    let message = format!("failed to parse config TOML from {source_name}");

    let Some(span) = error.span() else {
        return format!("{message}\ndetail: {error}");
    };

    let mut output = Vec::new();
    let report = Report::build(ReportKind::Error, (source_name, span.clone()))
        .with_config(
            AriadneConfig::default()
                .with_color(false)
                .with_char_set(CharSet::Ascii)
                .with_index_type(IndexType::Byte),
        )
        .with_message(&message)
        .with_label(Label::new((source_name, span)).with_message(error.message()))
        .finish();

    if report
        .write((source_name, Source::from(config_str)), &mut output)
        .is_ok()
    {
        String::from_utf8_lossy(&output).into_owned()
    } else {
        format!("{message}\ndetail: {error}")
    }
}

#[cfg(not(feature = "rich-config-errors"))]
fn format_toml_parse_error(
    source_name: &str,
    _config_str: &str,
    error: &toml::de::Error,
) -> String {
    format!("failed to parse config TOML from {source_name}: {error}")
}

#[derive(Debug, Clone)]
pub struct TomlConfig {
    config: Arc<Mutex<Config>>,
}

impl Default for TomlConfig {
    fn default() -> Self {
        TomlConfig::new_from_str("").unwrap()
    }
}

impl TomlConfig {
    /// Compact only values produced by the web form. General TOML dumps retain
    /// explicit values, including explicit defaults used in patches.
    #[cfg(feature = "config-write")]
    pub(crate) fn prune_form_defaults(&self) {
        let mut config = self.config.lock().unwrap();
        config.flags.canonicalize_p2p_disguise();
        {
            let mut supplied = serde_json::to_value(&config.flags)
                .expect("FlagsPatch has a JSON-compatible generated schema");
            let defaults = serde_json::to_value(Flags::defaults().downgrade())
                .expect("FlagsPatch defaults have a JSON-compatible generated schema");
            if let (Some(supplied), Some(defaults)) =
                (supplied.as_object_mut(), defaults.as_object())
            {
                for key in Flags::form() {
                    if supplied.get(*key) == defaults.get(*key) {
                        supplied.remove(*key);
                    }
                }
            }
            config.flags = serde_json::from_value(supplied)
                .expect("removing optional FlagsPatch fields preserves its schema");
        }
        if config.hostname.as_ref().is_some_and(String::is_empty) {
            config.hostname = None;
        }
        if config
            .sni
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            config.sni = None;
        }
        if config.dhcp == Some(DhcpConfig::Enabled(false)) {
            config.dhcp = None;
        }
        if config.dhcpv6 == Some(DhcpConfig::Enabled(false)) {
            config.dhcpv6 = None;
        }
        // An explicitly empty listeners/routes list disables implicit defaults.
        if config.mapped_listeners.as_ref().is_some_and(Vec::is_empty) {
            config.mapped_listeners = None;
        }
        if config.peer.as_ref().is_some_and(Vec::is_empty) {
            config.peer = None;
        }
        if config.proxy_network.as_ref().is_some_and(Vec::is_empty) {
            config.proxy_network = None;
        }
        if config.port_forward.as_ref().is_some_and(Vec::is_empty) {
            config.port_forward = None;
        }
        if config
            .vpn
            .as_ref()
            .is_some_and(|vpn| *vpn == crate::config::vpn::VpnConfig::default())
        {
            config.vpn = None;
        }
        if config.secure_mode.as_ref().is_some_and(|secure| {
            !secure.enabled
                && secure.local_private_key.is_none()
                && secure.local_public_key.is_none()
        }) {
            config.secure_mode = None;
        }
    }

    fn normalize_config_source(config: &mut Config) {
        if matches!(
            config.source.as_ref().map(|source| source.source),
            Some(ConfigSource::User)
        ) {
            config.source = None;
        }
    }

    #[cfg(feature = "config-write")]
    fn config_for_dump(&self) -> Config {
        let mut config = self.config.lock().unwrap().clone();
        Self::normalize_config_source(&mut config);
        if config.parallel_connections == Some(1) {
            config.parallel_connections = None;
        }
        if config.disable_parallel_connections == Some(false) {
            config.disable_parallel_connections = None;
        }
        if config.traffic_disguise.as_deref() == Some("speedtest") {
            config.traffic_disguise = None;
        }
        if config
            .vpn
            .as_ref()
            .is_some_and(|vpn| *vpn == crate::config::vpn::VpnConfig::default())
        {
            config.vpn = None;
        }
        config
    }

    #[cfg(feature = "config-write")]
    fn redact_secrets(config: &mut Config) {
        const REDACTED: &str = "<redacted>";

        if let Some(secret) = config
            .network_identity
            .as_mut()
            .and_then(|identity| identity.network_secret.as_mut())
            && !secret.is_empty()
        {
            *secret = REDACTED.to_owned();
        }
        if let Some(private_key) = config
            .secure_mode
            .as_mut()
            .and_then(|secure_mode| secure_mode.local_private_key.as_mut())
            && !private_key.is_empty()
        {
            *private_key = REDACTED.to_owned();
        }
        if let Some(private_key) = config
            .vpn_portal_config
            .as_mut()
            .and_then(|portal| portal.wireguard_private_key.as_mut())
            && !private_key.is_empty()
        {
            *private_key = REDACTED.to_owned();
        }
        if let Some(declarations) = config
            .acl
            .as_mut()
            .and_then(|acl| acl.acl_v1.as_mut())
            .and_then(|acl| acl.group.as_mut())
            .map(|group| &mut group.declares)
        {
            for declaration in declarations {
                if !declaration.group_secret.is_empty() {
                    declaration.group_secret = REDACTED.to_owned();
                }
            }
        }
        for credential in &mut config.managed_credentials {
            if !credential.credential_secret.is_empty() {
                credential.credential_secret = REDACTED.to_owned();
            }
        }
    }

    pub fn new_from_str(config_str: &str) -> Result<Self, anyhow::Error> {
        Self::new_from_str_with_source("inline config", config_str)
    }

    pub fn new_from_str_with_source(
        source_name: &str,
        config_str: &str,
    ) -> Result<Self, anyhow::Error> {
        let mut config = toml::de::from_str::<Config>(config_str).map_err(|err| {
            let message = format_toml_parse_error(source_name, config_str, &err);
            anyhow::Error::new(err).context(message)
        })?;

        Self::normalize_config_source(&mut config);

        Self::new_from_config(config).map_err(|err| {
            let message = format!("failed to load config from {source_name}: {err}");
            err.context(message)
        })
    }

    /// Load a startup document, ignoring invalid values while retaining the
    /// remaining fields. API edits should use the strict constructors instead.
    pub fn new_from_str_tolerant_with_source(
        source_name: &str,
        config_str: &str,
    ) -> Result<Self, anyhow::Error> {
        let (config, mut warnings) = tolerant::load(config_str)?;
        let config = Self::new_from_config(config)
            .with_context(|| format!("failed to load recovered config from {source_name}"))?;
        tolerant::sanitize_portal(&config, &mut warnings)?;
        for warning in warnings {
            // Do not log the input value or deserializer error: either can
            // contain network secrets or private keys.
            tracing::warn!(
                config_source = source_name,
                field = warning.path,
                reason = warning.reason,
                "Ignoring invalid configuration value and continuing startup"
            );
        }
        Ok(config)
    }

    fn new_from_config(mut config: Config) -> Result<Self, anyhow::Error> {
        validate_transport_tuning(
            config.parallel_connections,
            config.traffic_disguise.as_deref(),
        )?;
        if let Some(vpn) = &config.vpn {
            vpn.validate()
                .map_err(|error| anyhow::anyhow!("invalid [vpn] config: {error}"))?;
        }
        config.flags.validate_p2p_disguise()?;
        config.secure_mode = config
            .secure_mode
            .take()
            .map(normalize_secure_mode_config)
            .transpose()
            .context("failed to normalize [secure_mode] config")?;
        let has_network_identity = config.network_identity.is_some();

        let config = TomlConfig {
            config: Arc::new(Mutex::new(config)),
        };

        let old_ns = config.get_network_identity();

        // Detect credential mode: secure_mode enabled + no network_secret in TOML
        let is_credential = has_network_identity
            && config
                .get_secure_mode()
                .map(|sm| sm.enabled)
                .unwrap_or(false)
            && old_ns
                .network_secret
                .as_deref()
                .is_none_or(|s| s.is_empty());

        if is_credential {
            config.set_network_identity(NetworkIdentity::new_credential(old_ns.network_name));
        } else {
            config.set_network_identity(NetworkIdentity::new(
                old_ns.network_name,
                old_ns.network_secret.unwrap_or_default(),
            ));
        }

        Ok(config)
    }
}

#[cfg(feature = "web-client")]
mod snapshot;

impl ConfigLoader for TomlConfig {
    fn get_inst_name(&self) -> String {
        self.config
            .lock()
            .unwrap()
            .instance_name
            .clone()
            .unwrap_or_else(default_instance_name)
    }

    fn set_inst_name(&self, name: String) {
        self.config.lock().unwrap().instance_name = Some(name);
    }

    fn get_hostname(&self) -> String {
        let hostname = self.config.lock().unwrap().hostname.clone();

        match hostname {
            Some(hostname) => {
                let hostname = hostname
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(32)
                    .collect::<String>();

                if !hostname.is_empty() {
                    self.set_hostname(Some(hostname.clone()));
                    hostname
                } else {
                    self.set_hostname(None);
                    String::new()
                }
            }
            None => String::new(),
        }
    }

    fn set_hostname(&self, name: Option<String>) {
        self.config.lock().unwrap().hostname = name;
    }

    fn get_sni(&self) -> String {
        self.config
            .lock()
            .unwrap()
            .sni
            .clone()
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    fn set_sni(&self, sni: Option<String>) {
        self.config.lock().unwrap().sni = sni
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
    }

    fn get_parallel_connections(&self) -> u32 {
        self.config
            .lock()
            .unwrap()
            .parallel_connections
            .unwrap_or(1)
    }

    fn set_parallel_connections(&self, count: Option<u32>) {
        self.config.lock().unwrap().parallel_connections = count.filter(|count| *count != 1);
    }

    fn get_disable_parallel_connections(&self) -> bool {
        self.config
            .lock()
            .unwrap()
            .disable_parallel_connections
            .unwrap_or(false)
    }

    fn set_disable_parallel_connections(&self, disabled: Option<bool>) {
        self.config.lock().unwrap().disable_parallel_connections =
            disabled.filter(|disabled| *disabled);
    }

    fn get_traffic_disguise(&self) -> String {
        self.config
            .lock()
            .unwrap()
            .traffic_disguise
            .clone()
            .unwrap_or_else(|| "speedtest".to_owned())
    }

    fn set_traffic_disguise(&self, style: Option<String>) {
        self.config.lock().unwrap().traffic_disguise = style.filter(|style| style != "speedtest");
    }

    fn get_netns(&self) -> Option<String> {
        self.config.lock().unwrap().netns.clone()
    }

    fn set_netns(&self, ns: Option<String>) {
        self.config.lock().unwrap().netns = ns;
    }

    fn get_ipv4(&self) -> Option<cidr::Ipv4Inet> {
        let locked_config = self.config.lock().unwrap();
        locked_config
            .ipv4
            .as_ref()
            .and_then(|s| s.parse().ok())
            .map(|c: cidr::Ipv4Inet| {
                if c.network_length() == 32 {
                    cidr::Ipv4Inet::new(c.address(), 24).unwrap()
                } else {
                    c
                }
            })
    }

    fn set_ipv4(&self, addr: Option<cidr::Ipv4Inet>) {
        self.config.lock().unwrap().ipv4 = addr.map(|addr| addr.to_string());
    }

    fn get_ipv6(&self) -> Option<cidr::Ipv6Inet> {
        let locked_config = self.config.lock().unwrap();
        locked_config.ipv6.as_ref().and_then(|s| s.parse().ok())
    }

    fn set_ipv6(&self, addr: Option<cidr::Ipv6Inet>) {
        self.config.lock().unwrap().ipv6 = addr.map(|addr| addr.to_string());
    }

    fn get_dhcp_config(&self) -> DhcpConfig {
        self.config.lock().unwrap().dhcp.unwrap_or_default()
    }

    fn set_dhcp_config(&self, dhcp: DhcpConfig) {
        self.config.lock().unwrap().dhcp = Some(dhcp);
    }

    fn get_dhcpv6_config(&self) -> DhcpConfig<cidr::Ipv6Cidr> {
        self.config.lock().unwrap().dhcpv6.unwrap_or_default()
    }

    fn set_dhcpv6_config(&self, dhcp: DhcpConfig<cidr::Ipv6Cidr>) {
        self.config.lock().unwrap().dhcpv6 = Some(dhcp);
    }

    fn add_proxy_cidr(
        &self,
        cidr: cidr::Ipv4Cidr,
        mapped_cidr: Option<cidr::Ipv4Cidr>,
    ) -> Result<(), anyhow::Error> {
        let mut locked_config = self.config.lock().unwrap();
        if locked_config.proxy_network.is_none() {
            locked_config.proxy_network = Some(vec![]);
        }
        if let Some(mapped_cidr) = mapped_cidr.as_ref()
            && cidr.network_length() != mapped_cidr.network_length()
        {
            return Err(anyhow::anyhow!(
                "Mapped CIDR must have the same network length as the original CIDR: {} != {}",
                cidr.network_length(),
                mapped_cidr.network_length()
            ));
        }
        // insert if no duplicate
        if !locked_config
            .proxy_network
            .as_ref()
            .unwrap()
            .iter()
            .any(|c| c.cidr == cidr && c.mapped_cidr == mapped_cidr)
        {
            locked_config
                .proxy_network
                .as_mut()
                .unwrap()
                .push(ProxyNetworkConfig {
                    cidr,
                    mapped_cidr,
                    allow: None,
                });
        }
        Ok(())
    }

    fn remove_proxy_cidr(&self, cidr: cidr::Ipv4Cidr) {
        let mut locked_config = self.config.lock().unwrap();
        if let Some(proxy_cidrs) = &mut locked_config.proxy_network {
            proxy_cidrs.retain(|c| c.cidr != cidr);
        }
    }

    fn clear_proxy_cidrs(&self) {
        let mut locked_config = self.config.lock().unwrap();
        locked_config.proxy_network = None;
    }

    fn get_proxy_cidrs(&self) -> Vec<ProxyNetworkConfig> {
        self.config
            .lock()
            .unwrap()
            .proxy_network
            .as_ref()
            .cloned()
            .unwrap_or_default()
    }

    fn get_id(&self) -> uuid::Uuid {
        let mut locked_config = self.config.lock().unwrap();
        match locked_config.instance_id {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::new_v4();
                locked_config.instance_id = Some(id);
                id
            }
        }
    }

    fn set_id(&self, id: uuid::Uuid) {
        self.config.lock().unwrap().instance_id = Some(id);
    }

    fn get_network_identity(&self) -> NetworkIdentity {
        self.config
            .lock()
            .unwrap()
            .network_identity
            .clone()
            .unwrap_or_default()
    }

    fn set_network_identity(&self, identity: NetworkIdentity) {
        self.config.lock().unwrap().network_identity = Some(identity);
    }

    fn get_listener_uris(&self) -> Vec<url::Url> {
        self.config
            .lock()
            .unwrap()
            .listeners
            .clone()
            .unwrap_or_default()
    }

    fn get_peers(&self) -> Vec<PeerConfig> {
        self.config.lock().unwrap().peer.clone().unwrap_or_default()
    }

    fn set_peers(&self, peers: Vec<PeerConfig>) {
        self.config.lock().unwrap().peer = Some(peers);
    }

    fn get_listeners(&self) -> Option<Vec<url::Url>> {
        self.config.lock().unwrap().listeners.clone()
    }

    fn set_listeners(&self, listeners: Vec<url::Url>) {
        self.config.lock().unwrap().listeners = Some(listeners);
    }

    fn get_mapped_listeners(&self) -> Vec<url::Url> {
        self.config
            .lock()
            .unwrap()
            .mapped_listeners
            .clone()
            .unwrap_or_default()
    }

    fn set_mapped_listeners(&self, listeners: Option<Vec<url::Url>>) {
        self.config.lock().unwrap().mapped_listeners = listeners;
    }

    fn get_vpn_portal_config(&self) -> Option<VpnPortalConfig> {
        self.config.lock().unwrap().vpn_portal_config.clone()
    }
    fn set_vpn_portal_config(&self, config: VpnPortalConfig) {
        self.config.lock().unwrap().vpn_portal_config = Some(config);
    }

    fn get_flags(&self) -> Flags {
        Flags::resolve(self.get_flags_patch())
    }

    fn set_flags(&self, flags: Flags) {
        self.config.lock().unwrap().flags = flags.downgrade();
    }

    fn get_flags_patch(&self) -> FlagsPatch {
        self.config.lock().unwrap().flags.clone()
    }

    fn patch_flags(&self, mut flags: FlagsPatch) {
        // Protobuf merge: an update overwrites exactly the fields it carries,
        // and presence is the encoding, so a flag explicitly set to its default
        // value is applied like any other. Optionize's merge cannot express that
        // here, because the one flattened nullable field would have to assign
        // unconditionally.
        let mut config = self.config.lock().unwrap();
        let sets_mode = flags
            .p2p_disguise_mode
            .as_deref()
            .is_some_and(|mode| !mode.trim().is_empty());
        if sets_mode {
            // A unified policy replaces legacy overrides, even when an older
            // caller's projection includes both representations in one update.
            config.flags.prefer_wss_http3_for_p2p = None;
            config.flags.disable_wss_http3_for_p2p = None;
            config.flags.only_use_wss_http3_for_hole_punching = None;
            flags.prefer_wss_http3_for_p2p = None;
            flags.disable_wss_http3_for_p2p = None;
            flags.only_use_wss_http3_for_hole_punching = None;
        } else if (flags.prefer_wss_http3_for_p2p.is_some()
            || flags.disable_wss_http3_for_p2p.is_some()
            || flags.only_use_wss_http3_for_hole_punching.is_some())
            && config
                .flags
                .p2p_disguise_mode
                .as_deref()
                .is_some_and(|mode| mode.parse::<super::P2pDisguiseMode>().is_ok())
        {
            // Legacy CLI/API patches edit the effective boolean policy. Keep
            // untouched siblings, then retire the mode that would mask edits.
            let resolved = Flags::resolve(config.flags.clone());
            config.flags.prefer_wss_http3_for_p2p = Some(resolved.prefer_wss_http3_for_p2p);
            config.flags.disable_wss_http3_for_p2p = Some(resolved.disable_wss_http3_for_p2p);
            config.flags.only_use_wss_http3_for_hole_punching =
                Some(resolved.only_use_wss_http3_for_hole_punching);
            config.flags.p2p_disguise_mode = None;
        }
        let update = prost::Message::encode_to_vec(&flags);
        prost::Message::merge(&mut config.flags, update.as_slice())
            .expect("decoding the bytes just encoded cannot fail");
    }

    fn get_vpn(&self) -> crate::config::vpn::VpnConfig {
        self.config.lock().unwrap().vpn.clone().unwrap_or_default()
    }

    fn set_vpn(&self, vpn: crate::config::vpn::VpnConfig) {
        self.config.lock().unwrap().vpn = Some(vpn);
    }

    fn get_routes(&self) -> Option<Vec<cidr::Ipv4Cidr>> {
        self.config.lock().unwrap().routes.clone()
    }

    fn set_routes(&self, routes: Option<Vec<cidr::Ipv4Cidr>>) {
        self.config.lock().unwrap().routes = routes;
    }

    fn get_socks5_portal(&self) -> Option<url::Url> {
        self.config.lock().unwrap().socks5_proxy.clone()
    }

    fn set_socks5_portal(&self, addr: Option<url::Url>) {
        self.config.lock().unwrap().socks5_proxy = addr;
    }

    fn get_port_forwards(&self) -> Vec<PortForwardConfig> {
        self.config
            .lock()
            .unwrap()
            .port_forward
            .clone()
            .unwrap_or_default()
    }

    fn set_port_forwards(&self, forwards: Vec<PortForwardConfig>) {
        self.config.lock().unwrap().port_forward = Some(forwards);
    }

    fn get_acl(&self) -> Option<Acl> {
        self.config.lock().unwrap().acl.clone()
    }

    fn set_acl(&self, acl: Option<Acl>) {
        self.config.lock().unwrap().acl = acl;
    }

    fn get_tcp_whitelist(&self) -> Vec<String> {
        self.config
            .lock()
            .unwrap()
            .tcp_whitelist
            .clone()
            .unwrap_or_default()
    }

    fn set_tcp_whitelist(&self, whitelist: Vec<String>) {
        self.config.lock().unwrap().tcp_whitelist = Some(whitelist);
    }

    fn get_udp_whitelist(&self) -> Vec<String> {
        self.config
            .lock()
            .unwrap()
            .udp_whitelist
            .clone()
            .unwrap_or_default()
    }

    fn set_udp_whitelist(&self, whitelist: Vec<String>) {
        self.config.lock().unwrap().udp_whitelist = Some(whitelist);
    }

    fn get_stun_servers(&self) -> Option<Vec<String>> {
        self.config.lock().unwrap().stun_servers.clone()
    }

    fn set_stun_servers(&self, servers: Option<Vec<String>>) {
        self.config.lock().unwrap().stun_servers = servers;
    }

    fn get_tcp_stun_servers(&self) -> Option<Vec<String>> {
        self.config.lock().unwrap().tcp_stun_servers.clone()
    }

    fn set_tcp_stun_servers(&self, servers: Option<Vec<String>>) {
        self.config.lock().unwrap().tcp_stun_servers = servers;
    }

    fn get_stun_servers_v6(&self) -> Option<Vec<String>> {
        self.config.lock().unwrap().stun_servers_v6.clone()
    }

    fn set_stun_servers_v6(&self, servers: Option<Vec<String>>) {
        self.config.lock().unwrap().stun_servers_v6 = servers;
    }

    fn get_secure_mode(&self) -> Option<SecureModeConfig> {
        self.config.lock().unwrap().secure_mode.clone()
    }

    fn set_secure_mode(&self, secure_mode: Option<SecureModeConfig>) {
        self.config.lock().unwrap().secure_mode = secure_mode;
    }

    fn get_credential_file(&self) -> Option<PathBuf> {
        self.config.lock().unwrap().credential_file.clone()
    }

    fn set_credential_file(&self, path: Option<PathBuf>) {
        self.config.lock().unwrap().credential_file = path;
    }

    fn get_managed_credentials(&self) -> Vec<ManagedCredentialConfig> {
        self.config.lock().unwrap().managed_credentials.clone()
    }

    fn set_managed_credentials(&self, credentials: Vec<ManagedCredentialConfig>) {
        self.config.lock().unwrap().managed_credentials = credentials;
    }

    fn get_network_config_source(&self) -> ConfigSource {
        self.config
            .lock()
            .unwrap()
            .source
            .as_ref()
            .map(|source| source.source)
            .unwrap_or(ConfigSource::User)
    }

    fn set_network_config_source(&self, source: Option<ConfigSource>) {
        self.config.lock().unwrap().source = source.and_then(|source| match source {
            ConfigSource::User => None,
            other => Some(ConfigSourceConfig { source: other }),
        });
    }

    fn dump(&self) -> String {
        #[cfg(feature = "config-write")]
        {
            toml::to_string_pretty(&self.config_for_dump()).unwrap()
        }
        #[cfg(not(feature = "config-write"))]
        {
            panic!("this build does not include TOML configuration serialization")
        }
    }

    fn dump_redacted(&self) -> String {
        #[cfg(feature = "config-write")]
        {
            let mut config = self.config_for_dump();
            Self::redact_secrets(&mut config);
            toml::to_string_pretty(&config).unwrap()
        }
        #[cfg(not(feature = "config-write"))]
        {
            panic!("this build does not include TOML configuration serialization")
        }
    }
}

/// Transitional name retained while native consumers migrate to [`TomlConfig`].
pub type TomlConfigLoader = TomlConfig;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "config-write")]
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
    fn startup_recovers_invalid_parallel_tuning_independently() {
        let config = TomlConfig::new_from_str_tolerant_with_source("test", "hostname = 'keep'\nparallel_connections = 17\ntraffic_disguise = 'invalid'\ndisable_parallel_connections = true").unwrap();
        assert_eq!(config.get_hostname(), "keep");
        assert_eq!(config.get_parallel_connections(), 1);
        assert_eq!(config.get_traffic_disguise(), "speedtest");
        assert!(config.get_disable_parallel_connections());
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
        let config =
            TomlConfig::new_from_str("dhcp = '10.20.0.0/24'\ndhcpv6 = 'fd12::/64'").unwrap();
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
    fn dhcp_accepts_booleans_and_ipv4_subnets() {
        assert!(!TomlConfig::default().get_dhcp());
        for (value, expected) in [
            ("false", DhcpConfig::Enabled(false)),
            ("true", DhcpConfig::Enabled(true)),
            (
                "'192.168.100.0/24'",
                DhcpConfig::Cidr("192.168.100.0/24".parse().unwrap()),
            ),
            (
                "'192.168.100.0/30'",
                DhcpConfig::Cidr("192.168.100.0/30".parse().unwrap()),
            ),
            (
                "'0.0.0.0/0'",
                DhcpConfig::Cidr("0.0.0.0/0".parse().unwrap()),
            ),
        ] {
            let config = TomlConfig::new_from_str(&format!("dhcp = {value}")).unwrap();
            assert_eq!(config.get_dhcp_config(), expected);
            assert_eq!(config.get_dhcp(), expected.enabled());
            #[cfg(feature = "config-write")]
            assert_eq!(
                TomlConfig::new_from_str(&config.dump())
                    .unwrap()
                    .get_dhcp_config(),
                expected
            );
            config.set_dhcp(false);
            assert_eq!(config.get_dhcp_config(), DhcpConfig::Enabled(false));
        }
    }

    #[test]
    fn dhcpv6_config_round_trip_and_validation() {
        assert!(!TomlConfig::default().get_dhcpv6());
        for value in [
            "true",
            "false",
            "'fd12:3456::/64'",
            "'fd12::/126'",
            "'2001:db8::/48'",
        ] {
            let config =
                TomlConfig::new_from_str(&format!("dhcp = true\ndhcpv6 = {value}")).unwrap();
            let exported = config.dump();
            let restored = TomlConfig::new_from_str(&exported).unwrap();
            assert_eq!(restored.get_dhcpv6_config(), config.get_dhcpv6_config());
            assert!(restored.get_dhcp());
            assert!(exported.contains("dhcp = true"));
        }
        for value in [
            "yes",
            "10.0.0.0/24",
            "fd12::1/64",
            "fd12::/127",
            "fd12::/128",
            "::/0",
            "::/64",
            "fe80::/64",
            "ff00::/8",
        ] {
            assert!(
                TomlConfig::new_from_str(&format!("dhcpv6 = '{value}'")).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn dhcp_rejects_invalid_or_unusable_subnets() {
        for value in [
            "'yes'",
            "'true'",
            "'::/64'",
            "'192.168.100.0/31'",
            "'192.168.100.1/32'",
            "'192.168.100.1/24'",
            "'192.168.100.0/33'",
            "'192.168.100.1'",
            "123",
            "{}",
            "[]",
        ] {
            assert!(
                TomlConfig::new_from_str(&format!("dhcp = {value}")).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn parse_error_preserves_source_and_location() {
        for field in ["dhcp", "dhcpv6"] {
            let input = format!("{field} = \"yes\"");
            let error = TomlConfig::new_from_str_with_source("fixture.toml", &input).unwrap_err();
            let display = error.to_string();

            assert!(display.contains("fixture.toml"));
            assert!(display.contains(&input));
            assert!(display.contains("invalid DHCP CIDR"));
            assert!(
                error
                    .chain()
                    .any(|cause| cause.downcast_ref::<toml::de::Error>().is_some())
            );
        }
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn toml_round_trip_preserves_config_and_non_default_flags() {
        let config = TomlConfig::new_from_str(
            r#"
instance_name = "node-a"
instance_id = "018f85a8-a9d0-7d4c-b73d-4ab62c048a20"
hostname = "host-a"
sni = "www.cloudflare.com"
listeners = ["tcp://0.0.0.0:11010"]

[network_identity]
network_name = "network-a"
network_secret = "secret-a"

[flags]
mtu = 1420
socket_mark = 0
"#,
        )
        .unwrap();

        let dumped = config.dump();
        let restored = TomlConfig::new_from_str(&dumped).unwrap();

        assert_eq!(restored.get_id(), config.get_id());
        assert_eq!(restored.get_hostname(), "host-a");
        assert_eq!(restored.get_sni(), "www.cloudflare.com");
        assert_eq!(
            restored.get_network_identity(),
            config.get_network_identity()
        );
        assert_eq!(restored.get_listener_uris(), config.get_listener_uris());
        assert_eq!(restored.get_flags().mtu, 1420);
        assert_eq!(restored.get_flags().socket_mark, Some(0));
    }

    #[test]
    fn legacy_vpn_portal_client_cidr_is_rejected_explicitly() {
        let error = TomlConfig::new_from_str(
            r#"
[vpn_portal_config]
client_cidr = "10.14.14.0/24"
wireguard_listen = "0.0.0.0:51820"
"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("client_cidr"), "{error}");
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn vpn_portal_round_trip_and_redacted_dump_preserve_dump_semantics() {
        let config = TomlConfig::new_from_str(
            r#"
[network_identity]
network_name = "network-a"
network_secret = "network-secret"

[secure_mode]
enabled = true
local_private_key = "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE="

[vpn_portal_config]
wireguard_listen = "0.0.0.0:51820"
wireguard_private_key = "wireguard-private-key"

[[vpn_portal_config.clients]]
name = "alice"
virtual_ip = "10.144.144.10/24"
groups = ["staff"]

[acl.acl_v1.group]

[[acl.acl_v1.group.declares]]
group_name = "staff"
group_secret = "group-secret"
"#,
        )
        .unwrap();

        let dumped = config.dump();
        assert!(dumped.contains("network-secret"));
        assert!(dumped.contains("YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE="));
        assert!(dumped.contains("wireguard-private-key"));
        assert!(dumped.contains("group-secret"));
        assert_eq!(
            TomlConfig::new_from_str(&dumped)
                .unwrap()
                .get_vpn_portal_config(),
            config.get_vpn_portal_config()
        );

        let redacted = config.dump_redacted();
        assert!(!redacted.contains("network-secret"));
        assert!(!redacted.contains("YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE="));
        assert!(!redacted.contains("wireguard-private-key"));
        assert!(!redacted.contains("group-secret"));
        assert_eq!(redacted.matches("<redacted>").count(), 4);
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn managed_credentials_round_trip_and_redact_secret() {
        let config = TomlConfig::new_from_str(
            r#"
[[managed_credentials]]
credential_id = "managed-a"
credential_secret = "private-key-material"
groups = ["ops"]
allow_relay = true
allowed_proxy_cidrs = ["10.0.0.0/24"]
expiry_unix = 2000000000
"#,
        )
        .unwrap();

        let dumped = config.dump();
        let restored = TomlConfig::new_from_str(&dumped).unwrap();
        assert_eq!(
            restored.get_managed_credentials(),
            config.get_managed_credentials()
        );
        assert!(dumped.contains("private-key-material"));

        let redacted = config.dump_redacted();
        assert!(!redacted.contains("private-key-material"));
        assert!(redacted.contains("<redacted>"));
        assert!(!TomlConfig::default().dump().contains("managed_credentials"));
    }

    #[test]
    fn hostname_normalization_is_portable_and_has_no_host_fallback() {
        let absent = TomlConfig::default();
        assert_eq!(absent.get_hostname(), "");

        let configured = TomlConfig::new_from_str("hostname = \"node\\u0007-name\"").unwrap();
        assert_eq!(configured.get_hostname(), "node-name");
    }

    #[test]
    fn credential_mode_does_not_synthesize_a_network_secret() {
        let config = TomlConfig::new_from_str(
            r#"
[network_identity]
network_name = "credential-network"

[secure_mode]
enabled = true
"#,
        )
        .unwrap();

        let identity = config.get_network_identity();
        assert_eq!(identity.network_name, "credential-network");
        assert_eq!(identity.network_secret, None);
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn user_source_is_implicit_while_web_source_round_trips() {
        let user = TomlConfig::new_from_str(
            r#"
[source]
source = "user"
"#,
        )
        .unwrap();
        assert_eq!(user.get_network_config_source(), ConfigSource::User);
        assert!(!user.dump().contains("[source]"));

        let web = TomlConfig::new_from_str(
            r#"
[source]
source = "web"
"#,
        )
        .unwrap();
        assert_eq!(web.get_network_config_source(), ConfigSource::Web);
        assert!(web.dump().contains("source = \"web\""));
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    use crate::proto::common::CompressionAlgoPb;
    use base64::{Engine as _, prelude::BASE64_STANDARD};

    #[test]
    fn socket_mark_config_file_roundtrip_none_some_and_zero() {
        // Omitting the flag leaves socket_mark unset (None) -> SO_MARK untouched.
        let cfg = TomlConfigLoader::new_from_str(
            r#"
[network_identity]
network_name = "n"
network_secret = "s"
"#,
        )
        .unwrap();
        assert_eq!(cfg.get_flags().socket_mark, None);

        // socket_mark = 0 is a legitimate value distinct from "unset".
        let cfg = TomlConfigLoader::new_from_str(
            r#"
[network_identity]
network_name = "n"
network_secret = "s"

[flags]
socket_mark = 0
"#,
        )
        .unwrap();
        assert_eq!(cfg.get_flags().socket_mark, Some(0));

        // A non-zero mark round-trips as Some(v).
        let cfg = TomlConfigLoader::new_from_str(
            r#"
[network_identity]
network_name = "n"
network_secret = "s"

[flags]
socket_mark = 66
"#,
        )
        .unwrap();
        assert_eq!(cfg.get_flags().socket_mark, Some(66));

        // Replacing flags with an unset socket mark must not resurrect the
        // previous value when the configuration is dumped and reloaded.
        cfg.set_flags(Flags {
            socket_mark: None,
            ..cfg.get_flags()
        });
        assert_eq!(cfg.get_flags().socket_mark, None);
        #[cfg(feature = "config-write")]
        assert_eq!(
            TomlConfigLoader::new_from_str(&cfg.dump())
                .unwrap()
                .get_flags()
                .socket_mark,
            None
        );
    }

    #[test]
    fn legacy_flag_patches_edit_unified_disguise_policy_and_preserve_siblings() {
        let config = TomlConfig::new_from_str("[flags]\np2p_disguise_mode = 'disabled'\n").unwrap();
        config.patch_flags(FlagsPatch {
            disable_wss_http3_for_p2p: Some(false),
            ..Default::default()
        });
        let flags = config.get_flags();
        assert!(!flags.prefer_wss_http3_for_p2p);
        assert!(!flags.disable_wss_http3_for_p2p);
        assert!(!flags.only_use_wss_http3_for_hole_punching);
        assert_eq!(config.get_flags_patch().p2p_disguise_mode, None);

        let config = TomlConfig::new_from_str("[flags]\np2p_disguise_mode = 'only'\n").unwrap();
        config.patch_flags(FlagsPatch {
            prefer_wss_http3_for_p2p: Some(true),
            ..Default::default()
        });
        let flags = config.get_flags();
        assert!(flags.prefer_wss_http3_for_p2p);
        assert!(flags.only_use_wss_http3_for_hole_punching);
        assert!(!flags.disable_wss_http3_for_p2p);
        assert_eq!(config.get_flags_patch().p2p_disguise_mode, None);
    }

    #[test]
    fn unified_disguise_patch_replaces_legacy_overrides_but_unrelated_patch_keeps_mode() {
        let config =
            TomlConfig::new_from_str("[flags]\nonly_use_wss_http3_for_hole_punching = true\n")
                .unwrap();
        config.patch_flags(FlagsPatch {
            p2p_disguise_mode: Some("off".into()),
            disable_wss_http3_for_p2p: Some(true),
            ..Default::default()
        });
        let patch = config.get_flags_patch();
        assert_eq!(patch.p2p_disguise_mode.as_deref(), Some("off"));
        assert_eq!(patch.prefer_wss_http3_for_p2p, None);
        assert_eq!(patch.disable_wss_http3_for_p2p, None);
        assert_eq!(patch.only_use_wss_http3_for_hole_punching, None);
        let flags = config.get_flags();
        assert!(!flags.prefer_wss_http3_for_p2p);
        assert!(!flags.disable_wss_http3_for_p2p);
        assert!(!flags.only_use_wss_http3_for_hole_punching);

        config.patch_flags(FlagsPatch {
            latency_first: Some(true),
            ..Default::default()
        });
        assert_eq!(
            config.get_flags_patch().p2p_disguise_mode.as_deref(),
            Some("off")
        );
        assert!(config.get_flags().latency_first);
    }

    #[test]
    fn contradictory_disguise_flags_are_rejected_on_load() {
        let error = TomlConfig::new_from_str(
            "[flags]\nonly_use_wss_http3_for_hole_punching = true\ndisable_wss_http3_for_p2p = true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("mutually exclusive"), "{error}");

        for input in [
            "only_use_wss_http3_for_hole_punching = true",
            "disable_wss_http3_for_p2p = true",
            "only_use_wss_http3_for_hole_punching = false\ndisable_wss_http3_for_p2p = true",
        ] {
            TomlConfig::new_from_str(&format!("[flags]\n{input}\n"))
                .unwrap_or_else(|error| panic!("single disguise flag must load: {error}"));
        }
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn flags_presence_survives_parse_patch_and_dump() {
        for (input, expected) in [
            ("", None),
            ("enable_encryption = true", Some(true)),
            ("enable_encryption = false", Some(false)),
        ] {
            let config = TomlConfig::new_from_str(&format!("[flags]\n{input}")).unwrap();
            assert_eq!(config.get_flags_patch().enable_encryption, expected);
            config.patch_flags(FlagsPatch {
                latency_first: Some(false),
                ..Default::default()
            });
            let restored = TomlConfig::new_from_str(&config.dump()).unwrap();
            assert_eq!(restored.get_flags_patch().enable_encryption, expected);
            assert_eq!(restored.get_flags_patch().latency_first, Some(false));
            assert_eq!(restored.get_flags_patch().mtu, None);
            assert_eq!(
                restored.get_flags().enable_encryption,
                expected.unwrap_or(true)
            );
        }
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
            only_use_wss_http3_for_hole_punching: Some(false),
            prefer_wss_http3_for_p2p: Some(false),
            disable_wss_http3_for_p2p: Some(false),
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
                TomlConfig::new_from_str(&format!("[flags]\ndefault_protocol = {protocol:?}"))
                    .unwrap();
            assert_eq!(
                config.get_flags_patch().default_protocol.as_deref(),
                Some(protocol)
            );
            assert_eq!(config.get_flags().default_protocol, "udp");
        }
    }

    #[test]
    fn flags_accept_protobuf_aliases_and_numbers_without_default_collisions() {
        let config = TomlConfig::new_from_str(
            r#"[flags]
enableEncryption = false
mtu = "1420"
foreignRelayBpsLimit = "18446744073709551615"
dataCompressAlgo = "Zstd"
socketMark = "0"
"#,
        )
        .unwrap();
        let flags = config.get_flags();
        assert!(!flags.enable_encryption);
        assert_eq!(flags.mtu, 1420);
        assert_eq!(flags.foreign_relay_bps_limit, u64::MAX);
        assert_eq!(flags.data_compress_algo, CompressionAlgoPb::Zstd as i32);
        assert_eq!(flags.socket_mark, Some(0));
        for fields in [
            "enableEncryption = true\nenable_encryption = false",
            "mtu = -1",
            "mtu = 4294967296",
            "unknown_flag = true",
            "accept_dns = true",
            "tld_dns_zone = \"et.net.\"",
            "data_compress_algo = 99",
        ] {
            assert!(
                TomlConfig::new_from_str(&format!("[flags]\n{fields}")).is_err(),
                "{fields}"
            );
        }
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn dump_omits_all_default_flags() {
        let config = TomlConfigLoader::default();
        let dumped = config.dump();
        let document: toml::Table = toml::from_str(&dumped).unwrap();
        assert!(!document.contains_key("flags"));
        assert_eq!(
            TomlConfigLoader::new_from_str(&dumped).unwrap().get_flags(),
            Flags::resolve(FlagsPatch::default())
        );
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn dump_preserves_flags_that_differ_from_easytier_defaults() {
        let cfg = TomlConfigLoader::default();
        let mut flags = Flags::resolve(FlagsPatch::default());
        flags.dev_name = "et_test".to_string();
        flags.enable_quic_proxy = true;
        flags.disable_tcp_hole_punching = true;
        flags.disable_sym_hole_punching = true;
        flags.multi_thread = false;
        flags.bind_device = false;
        flags.enable_ipv6 = false;
        flags.relay_network_whitelist = "".to_string();
        flags.prefer_peer_relay = true;
        flags.mtu = 0;
        flags.foreign_relay_bps_limit = u64::MAX - 1;
        flags.instance_recv_bps_limit = u64::MAX - 2;
        flags.data_compress_algo = CompressionAlgoPb::Zstd.into();
        flags.socket_mark = Some(0);
        cfg.set_flags(flags);

        let dumped = cfg.dump();

        assert!(dumped.contains("dev_name = \"et_test\""));
        assert!(dumped.contains("enable_quic_proxy = true"));
        assert!(dumped.contains("disable_tcp_hole_punching = true"));
        assert!(dumped.contains("disable_sym_hole_punching = true"));
        assert!(dumped.contains("multi_thread = false"));
        assert!(dumped.contains("bind_device = false"));
        assert!(dumped.contains("enable_ipv6 = false"));
        assert!(dumped.contains("relay_network_whitelist = \"\""));
        assert!(dumped.contains("mtu = 0"));
        assert!(dumped.contains("foreign_relay_bps_limit = \"18446744073709551614\""));
        assert!(dumped.contains("instance_recv_bps_limit = \"18446744073709551613\""));
        assert!(dumped.contains("data_compress_algo = \"Zstd\""));
        assert!(dumped.contains("socket_mark = 0"));

        let reloaded = TomlConfigLoader::new_from_str(&dumped).unwrap();
        let reloaded_flags = reloaded.get_flags();
        assert_eq!(reloaded_flags.dev_name, "et_test");
        assert!(reloaded_flags.enable_quic_proxy);
        assert!(reloaded_flags.disable_tcp_hole_punching);
        assert!(reloaded_flags.disable_sym_hole_punching);
        assert!(!reloaded_flags.multi_thread);
        assert!(!reloaded_flags.bind_device);
        assert!(!reloaded_flags.enable_ipv6);
        assert_eq!(reloaded_flags.relay_network_whitelist, "");
        assert!(reloaded_flags.prefer_peer_relay);
        assert_eq!(reloaded_flags.mtu, 0);
        assert_eq!(reloaded_flags.foreign_relay_bps_limit, u64::MAX - 1);
        assert_eq!(reloaded_flags.instance_recv_bps_limit, u64::MAX - 2);
        assert_eq!(
            reloaded_flags.data_compress_algo,
            i32::from(CompressionAlgoPb::Zstd)
        );
        assert_eq!(reloaded_flags.socket_mark, Some(0));
    }

    #[test]
    fn test_stun_servers_config() {
        let config = TomlConfigLoader::default();
        let stun_servers = config.get_stun_servers();
        assert!(stun_servers.is_none());
        assert!(config.get_tcp_stun_servers().is_none());

        // Test setting custom stun servers
        let custom_servers = vec!["txt:stun.easytier.cn".to_string()];
        config.set_stun_servers(Some(custom_servers.clone()));

        let retrieved_servers = config.get_stun_servers();
        assert_eq!(retrieved_servers.unwrap(), custom_servers);

        let custom_tcp_servers = vec!["tcp-stun.example.com:3478".to_string()];
        config.set_tcp_stun_servers(Some(custom_tcp_servers.clone()));

        let retrieved_tcp_servers = config.get_tcp_stun_servers();
        assert_eq!(retrieved_tcp_servers.unwrap(), custom_tcp_servers);
    }

    #[test]
    fn test_stun_servers_toml_parsing() {
        let config_str = r#"
instance_name = "test"
stun_servers = [
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "txt:stun.easytier.cn"
]
tcp_stun_servers = [
    "tcp-stun.example.com:3478"
]"#;

        let config = TomlConfigLoader::new_from_str(config_str).unwrap();
        let stun_servers = config.get_stun_servers().unwrap();
        let tcp_stun_servers = config.get_tcp_stun_servers().unwrap();

        assert_eq!(stun_servers.len(), 3);
        assert_eq!(stun_servers[0], "stun.l.google.com:19302");
        assert_eq!(stun_servers[1], "stun1.l.google.com:19302");
        assert_eq!(stun_servers[2], "txt:stun.easytier.cn");
        assert_eq!(tcp_stun_servers, ["tcp-stun.example.com:3478"]);
    }

    #[test]
    fn test_empty_tcp_stun_servers_toml_parsing() {
        let config = TomlConfigLoader::new_from_str(
            r#"
instance_name = "test"
tcp_stun_servers = []
"#,
        )
        .unwrap();

        assert_eq!(config.get_tcp_stun_servers(), Some(Vec::new()));
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn test_network_config_source_toml_roundtrip() {
        let config = TomlConfigLoader::default();
        assert_eq!(config.get_network_config_source(), ConfigSource::User);

        config.set_network_config_source(Some(ConfigSource::Web));
        let dumped = config.dump();

        assert!(dumped.contains("[source]"));
        assert!(dumped.contains("source = \"web\""));

        let loaded = TomlConfigLoader::new_from_str(&dumped).unwrap();
        assert_eq!(loaded.get_network_config_source(), ConfigSource::Web);
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn test_toml_credential_mode_omits_network_secret() {
        for network_secret in ["", r#"network_secret = """#] {
            let config = TomlConfigLoader::new_from_str(&format!(
                r#"
[network_identity]
network_name = "credential-network"
{network_secret}

[secure_mode]
enabled = true
"#
            ))
            .unwrap();

            let identity = config.get_network_identity();
            assert_eq!(identity.network_name, "credential-network");
            assert_eq!(identity.network_secret, None);
            assert_eq!(identity.network_secret_digest, None);
            assert!(!config.dump().contains("network_secret"));
        }
    }

    #[test]
    fn test_toml_secure_mode_without_network_identity_uses_default_secret() {
        let config = TomlConfigLoader::new_from_str(
            r#"
[secure_mode]
enabled = true
"#,
        )
        .unwrap();

        let identity = config.get_network_identity();
        assert_eq!(identity.network_name, "default");
        assert_eq!(identity.network_secret.as_deref(), Some(""));
        assert!(identity.network_secret_digest.is_some());
    }

    #[test]
    fn test_toml_secure_mode_generates_keypair_when_keys_missing() {
        let config = TomlConfigLoader::new_from_str(
            r#"
[secure_mode]
enabled = true
"#,
        )
        .unwrap();

        let secure_mode = config.get_secure_mode().unwrap();
        let private_key = secure_mode.private_key().unwrap();
        let public_key = secure_mode.public_key().unwrap();
        assert_eq!(
            x25519_dalek::PublicKey::from(&private_key).as_bytes(),
            public_key.as_bytes()
        );
    }

    #[test]
    fn test_toml_secure_mode_derives_public_key_from_private_key() {
        let private = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let config = TomlConfigLoader::new_from_str(&format!(
            r#"
[secure_mode]
enabled = true
local_private_key = "{}"
"#,
            BASE64_STANDARD.encode(private.as_bytes())
        ))
        .unwrap();

        let secure_mode = config.get_secure_mode().unwrap();
        let private_key = secure_mode.private_key().unwrap();
        assert_eq!(private_key.as_bytes(), private.as_bytes());
        assert_eq!(
            secure_mode.public_key().unwrap().as_bytes(),
            x25519_dalek::PublicKey::from(&private).as_bytes()
        );
    }

    #[test]
    fn test_toml_secure_mode_rejects_mismatched_keypair() {
        let private = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let other_public = x25519_dalek::PublicKey::from(
            &x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng),
        );
        let error = TomlConfigLoader::new_from_str(&format!(
            r#"
[secure_mode]
enabled = true
local_private_key = "{}"
local_public_key = "{}"
"#,
            BASE64_STANDARD.encode(private.as_bytes()),
            BASE64_STANDARD.encode(other_public.as_bytes())
        ))
        .unwrap_err();
        let error = format!("{error:#}");

        assert!(
            error.contains("failed to normalize [secure_mode] config"),
            "{error}"
        );
        assert!(
            error.contains("does not match generated public key"),
            "{error}"
        );
    }

    #[test]
    fn test_toml_secure_mode_disabled_keeps_keys_unset() {
        let config = TomlConfigLoader::new_from_str(
            r#"
[secure_mode]
enabled = false
"#,
        )
        .unwrap();

        let secure_mode = config.get_secure_mode().unwrap();
        assert!(!secure_mode.enabled);
        assert_eq!(secure_mode.local_private_key, None);
        assert_eq!(secure_mode.local_public_key, None);
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn test_toml_secure_mode_keypair_survives_roundtrip() {
        let config = TomlConfigLoader::new_from_str(
            r#"
[secure_mode]
enabled = true
"#,
        )
        .unwrap();

        let dumped = config.dump();
        let restored = TomlConfigLoader::new_from_str(&dumped).unwrap();

        assert_eq!(
            config.get_secure_mode().unwrap(),
            restored.get_secure_mode().unwrap()
        );
    }

    #[test]
    fn test_acl_toml_rule_uses_defaults_for_omitted_fields() {
        use crate::proto::acl::{Action, ChainType, Protocol};

        let config_str = r#"
[[acl.acl_v1.chains]]
name = "subnet_proxy_protect"
chain_type = 3
enabled = true
default_action = 2

[[acl.acl_v1.chains.rules]]
name = "allow_my_devices"
priority = 1000
action = 1
source_ips = ["10.172.192.2/32"]
protocol = 5
enabled = true
"#;

        let config = TomlConfigLoader::new_from_str(config_str).unwrap();
        let acl = config.get_acl().unwrap();
        let acl_v1 = acl.acl_v1.unwrap();
        let chain = &acl_v1.chains[0];
        let rule = &chain.rules[0];

        assert_eq!(chain.chain_type, ChainType::Forward as i32);
        assert_eq!(chain.default_action, Action::Drop as i32);
        assert_eq!(rule.action, Action::Allow as i32);
        assert_eq!(rule.protocol, Protocol::Any as i32);
        assert_eq!(rule.source_ips, vec!["10.172.192.2/32"]);
        assert!(rule.ports.is_empty());
        assert!(rule.source_ports.is_empty());
        assert!(rule.destination_ips.is_empty());
        assert!(rule.source_groups.is_empty());
        assert!(rule.destination_groups.is_empty());
        assert_eq!(rule.rate_limit, 0);
        assert_eq!(rule.burst_limit, 0);
        assert!(!rule.stateful);
    }

    #[test]
    fn test_acl_toml_group_can_omit_declares_or_members() {
        let declares_only = r#"
[acl.acl_v1.group]

[[acl.acl_v1.group.declares]]
group_name = "admin"
group_secret = "admin-pw"
"#;
        let config = TomlConfigLoader::new_from_str(declares_only).unwrap();
        let group = config.get_acl().unwrap().acl_v1.unwrap().group.unwrap();
        assert_eq!(group.declares.len(), 1);
        assert!(group.members.is_empty());

        let members_only = r#"
[acl.acl_v1.group]
members = ["admin"]
"#;
        let config = TomlConfigLoader::new_from_str(members_only).unwrap();
        let group = config.get_acl().unwrap().acl_v1.unwrap().group.unwrap();
        assert!(group.declares.is_empty());
        assert_eq!(group.members, vec!["admin"]);
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn test_vpn_config_roundtrip() {
        let config = TomlConfigLoader::default();
        let mut vpn = crate::config::vpn::VpnConfig::default();
        vpn.rules.push(crate::config::vpn::VpnRuleConfig {
            cidr: "0.0.0.0/0".parse().unwrap(),
            exit: "10.144.144.2".parse().unwrap(),
        });

        config.set_vpn(vpn.clone());
        assert_eq!(config.get_vpn(), vpn);

        let dumped = config.dump();
        let loaded = TomlConfigLoader::new_from_str(&dumped).unwrap();
        assert_eq!(loaded.get_vpn(), vpn);
    }
}

#[cfg(test)]
mod full_example_tests {
    use super::*;

    #[cfg(feature = "config-write")]
    #[test]
    fn full_example_test() {
        let config_str = r#"
instance_name = "default"
instance_id = "87ede5a2-9c3d-492d-9bbe-989b9d07e742"
ipv4 = "10.144.144.10"
listeners = [ "tcp://0.0.0.0:11010", "udp://0.0.0.0:11010" ]
routes = [ "192.168.0.0/16" ]

[network_identity]
network_name = "default"
network_secret = ""

[[peer]]
uri = "tcp://public.kkrainbow.top:11010"

[[peer]]
uri = "udp://192.168.94.33:11010"

[[proxy_network]]
cidr = "10.147.223.0/24"
allow = ["tcp", "udp", "icmp"]

[[proxy_network]]
cidr = "10.1.1.0/24"
allow = ["tcp", "icmp"]

[file_logger]
level = "info"
file = "easytier"
dir = "/tmp/easytier"

[console_logger]
level = "warn"

[[port_forward]]
bind_addr = "0.0.0.0:11011"
dst_addr = "192.168.94.33:11011"
proto = "tcp"
"#;
        let ret = TomlConfigLoader::new_from_str(config_str);
        if let Err(e) = &ret {
            println!("{}", e);
        } else {
            println!("{:?}", ret.as_ref().unwrap());
        }
        assert!(ret.is_ok());

        let ret = ret.unwrap();
        assert_eq!("10.144.144.10/24", ret.get_ipv4().unwrap().to_string());

        assert_eq!(
            vec!["tcp://0.0.0.0:11010", "udp://0.0.0.0:11010"],
            ret.get_listener_uris()
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<String>>()
        );

        assert_eq!(
            vec![PortForwardConfig {
                bind_addr: "0.0.0.0:11011".parse().unwrap(),
                dst_addr: "192.168.94.33:11011".parse().unwrap(),
                proto: "tcp".to_string(),
            }],
            ret.get_port_forwards()
        );
        println!("{}", ret.dump());
    }
}

#[cfg(test)]
mod diagnostic_compatibility_tests {
    use super::*;

    #[test]
    fn stdin_source_name_and_caret_are_preserved() {
        let error = TomlConfig::new_from_str_with_source("stdin", "dhcp = \"yes\"")
            .unwrap_err()
            .to_string();

        assert!(error.contains("stdin"));
        assert!(error.contains("dhcp = \"yes\""));
        assert!(error.contains('^'));
        assert!(!error.contains("<unknown>"));
    }

    #[test]
    fn non_ascii_before_typed_error_keeps_byte_location() {
        for field in ["dhcp", "dhcpv6"] {
            let invalid_field = format!("{field} = \"yes\"");
            let error = TomlConfig::new_from_str(&format!("hostname = \"节点\"\n{invalid_field}"))
                .unwrap_err()
                .to_string();

            assert!(error.contains(&invalid_field));
            assert!(error.contains('^'));
            assert!(error.contains("invalid DHCP CIDR"));
        }
    }

    #[cfg(feature = "rich-config-errors")]
    #[test]
    fn non_ascii_on_syntax_error_line_keeps_source_location() {
        let error = TomlConfig::new_from_str("hostname = \"节点\" dhcp = \"yes\"")
            .unwrap_err()
            .to_string();

        assert!(error.contains("inline config:1:"));
        assert!(error.contains("hostname = \"节点\" dhcp = \"yes\""));
        assert!(error.contains("expected newline"));
        assert!(!error.contains("<unknown>"));
    }

    #[test]
    fn flags_parse_error_keeps_source_span_and_cause_chain() {
        let error = TomlConfig::new_from_str_with_source(
            "flags-fixture.toml",
            "[flags]\nsocket_mark = \"bad\"",
        )
        .unwrap_err();
        let display = error.to_string();

        assert!(display.contains("flags-fixture.toml"));
        assert!(display.contains("failed to parse config TOML"));
        assert!(display.contains("socket_mark = \"bad\""));
        assert!(display.contains('^'));
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<toml::de::Error>().is_some())
        );
    }
}
