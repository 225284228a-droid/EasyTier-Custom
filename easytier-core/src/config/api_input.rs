//! Conversion between the management NetworkConfig schema and shared TOML.

use std::net::SocketAddr;

use anyhow::Context;
use easytier_proto::{api::manage, common::FlagsPatch};

#[cfg(all(
    feature = "browser-config",
    any(test, all(target_arch = "wasm32", target_os = "unknown"))
))]
use easytier_proto::common::Flags;
use optionize::{Optionizable, Optionized};

use crate::config::{
    MappedListenerPolicy, normalize_secure_mode_config,
    toml::{
        ConfigLoader, NetworkIdentity, PeerConfig, PortForwardConfig, TomlConfigLoader,
        VpnPortalClientConfig, VpnPortalConfig,
    },
};

fn parse_mapped_listener_urls(mapped_listeners: &[String]) -> Result<Vec<url::Url>, anyhow::Error> {
    MappedListenerPolicy::new(["tcp", "udp", "wg", "quic", "http3", "ws", "wss", "faketcp"])
        .parse_urls(mapped_listeners)
}

pub fn add_proxy_network_to_config(
    proxy_network: &str,
    cfg: &TomlConfigLoader,
) -> Result<(), anyhow::Error> {
    let parts: Vec<&str> = proxy_network.split("->").collect();
    let real_cidr = parts[0]
        .parse()
        .with_context(|| format!("failed to parse proxy network: {}", parts[0]))?;

    if parts.len() > 2 {
        return Err(anyhow::anyhow!(
                    "invalid proxy network format: {}, support format: <real_cidr> or <real_cidr>-><mapped_cidr>, example:
                    10.0.0.0/24 or 10.0.0.0/24->192.168.0.0/24",
                    proxy_network
                ));
    }

    let mapped_cidr = if parts.len() == 2 {
        Some(
            parts[1]
                .parse()
                .with_context(|| format!("failed to parse mapped network: {}", parts[1]))?,
        )
    } else {
        None
    };
    cfg.add_proxy_cidr(real_cidr, mapped_cidr)?;
    Ok(())
}

pub type NetworkingMethod = easytier_proto::api::manage::NetworkingMethod;
pub type NetworkConfig = easytier_proto::api::manage::NetworkConfig;

pub(crate) fn set_network_flags(result: &mut NetworkConfig, flags: FlagsPatch) {
    result.latency_first = flags.latency_first;
    result.dev_name = flags.dev_name;
    result.use_smoltcp = flags.use_smoltcp;
    result.disable_ipv6 = flags.enable_ipv6.map(|enabled| !enabled);
    result.enable_kcp_proxy = flags.enable_kcp_proxy;
    result.disable_kcp_input = flags.disable_kcp_input;
    result.enable_quic_proxy = flags.enable_quic_proxy;
    result.disable_quic_input = flags.disable_quic_input;
    result.disable_p2p = flags.disable_p2p;
    result.p2p_only = flags.p2p_only;
    result.lazy_p2p = flags.lazy_p2p;
    result.bind_device = flags.bind_device;
    result.socket_mark = flags.socket_mark;
    result.no_tun = flags.no_tun;
    result.relay_all_peer_rpc = flags.relay_all_peer_rpc;
    result.need_p2p = flags.need_p2p;
    result.multi_thread = flags.multi_thread;
    result.proxy_forward_by_system = flags.proxy_forward_by_system;
    result.disable_encryption = flags.enable_encryption.map(|enabled| !enabled);
    result.disable_tcp_hole_punching = flags.disable_tcp_hole_punching;
    result.disable_udp_hole_punching = flags.disable_udp_hole_punching;
    result.disable_upnp = flags.disable_upnp;
    result.disable_relay_data = flags.disable_relay_data;
    result.prefer_peer_relay = flags.prefer_peer_relay;
    result.enable_udp_broadcast_relay = flags.enable_udp_broadcast_relay;
    result.disable_sym_hole_punching = flags.disable_sym_hole_punching;
    result.p2p_disguise_mode = flags
        .p2p_disguise_mode
        .filter(|mode| !mode.trim().is_empty())
        .map(|mode| mode.trim().to_ascii_lowercase());
    result.p2p_prefer_protocol = flags.default_protocol;
    result.close_redundant_conns_when_disguised = flags.close_redundant_conns_when_disguised;
    result.mtu = flags.mtu.map(|mtu| mtu as i32);
    result.data_compress_algo = flags.data_compress_algo;
    result.encryption_algorithm = flags.encryption_algorithm;
    result.instance_recv_bps_limit = flags.instance_recv_bps_limit;
    result.enable_private_mode = flags.private_mode;

    result.enable_relay_network_whitelist = flags
        .relay_network_whitelist
        .as_ref()
        .map(|list| list != "*");
    result.relay_network_whitelist = flags
        .relay_network_whitelist
        .filter(|list| list != "*")
        .map(|list| list.split_whitespace().map(ToOwned::to_owned).collect())
        .unwrap_or_default();
}

pub trait NetworkConfigExt {
    fn gen_config(&self) -> Result<TomlConfigLoader, anyhow::Error>;
    fn new_from_config(config: impl ConfigLoader) -> Result<NetworkConfig, anyhow::Error>;
}

#[cfg(all(
    feature = "browser-config",
    any(test, all(target_arch = "wasm32", target_os = "unknown"))
))]
const FORM_MANAGED_TOML_FIELDS: &[&str] = &[
    "hostname",
    "sni",
    "parallel_connections",
    "disable_parallel_connections",
    "traffic_disguise",
    "instance_id",
    "ipv4",
    "vpn",
    "dhcp",
    "dhcpv6",
    "network_identity",
    "listeners",
    "mapped_listeners",
    "peer",
    "proxy_network",
    "vpn_portal_config",
    "routes",
    "socks5_proxy",
    "port_forward",
    "secure_mode",
    "acl",
    "credential_file",
    "managed_credentials",
];

#[cfg(all(
    feature = "browser-config",
    any(test, all(target_arch = "wasm32", target_os = "unknown"))
))]
pub(crate) fn merge_network_config_toml(
    original_toml: &str,
    config: &NetworkConfig,
) -> Result<String, anyhow::Error> {
    let generated_toml = config.gen_config()?.dump();
    let mut original = toml::from_str::<toml::Table>(original_toml)
        .context("failed to parse the original TOML document")?;
    let mut generated = toml::from_str::<toml::Table>(&generated_toml)
        .context("failed to parse the generated TOML document")?;

    for (key, value) in original.iter() {
        if key != "flags" && !FORM_MANAGED_TOML_FIELDS.contains(&key.as_str()) {
            generated.insert(key.clone(), value.clone());
        }
    }
    if !original.contains_key("instance_name") {
        generated.remove("instance_name");
    }

    let mut merged_flags: toml::Table = original
        .remove("flags")
        .and_then(|value| value.try_into().ok())
        .unwrap_or_default();
    let generated_flags: toml::Table = generated
        .remove("flags")
        .and_then(|value| value.try_into().ok())
        .unwrap_or_default();

    // Normalize any camelCase aliases in original [flags] to their canonical field names
    // so each field appears at most once in the merged table.
    for field in Flags::flags() {
        if let (Some(name), Some(json_name)) = (field.name.as_deref(), field.json_name.as_deref())
            && name != json_name
            && let Some(val) = merged_flags.remove(json_name)
        {
            merged_flags.entry(name.to_owned()).or_insert(val);
        }
    }

    // The keys a config form owns are the flags it offers a control for, which
    // the schema declares with `(easytier.flag)`.
    for key in Flags::form() {
        if let Some(value) = generated_flags.get(*key) {
            merged_flags.insert((*key).to_owned(), value.clone());
        } else {
            merged_flags.remove(*key);
        }
    }
    if !merged_flags.is_empty() {
        generated.insert("flags".to_owned(), toml::Value::Table(merged_flags));
    }

    toml::to_string_pretty(&generated).context("failed to serialize the merged TOML document")
}

fn parse_peer(peer: &manage::NetworkPeerConfig) -> Result<Option<PeerConfig>, anyhow::Error> {
    let uri = peer.uri.trim();
    if uri.is_empty() {
        return Ok(None);
    }

    Ok(Some(PeerConfig {
        uri: uri
            .parse()
            .with_context(|| format!("failed to parse peer uri: {}", uri))?,
        peer_public_key: peer.peer_public_key.clone(),
    }))
}

fn parse_peers(peers: &[manage::NetworkPeerConfig]) -> Result<Vec<PeerConfig>, anyhow::Error> {
    let mut ret = Vec::new();
    for peer in peers {
        if let Some(peer) = parse_peer(peer)? {
            ret.push(peer);
        }
    }
    Ok(ret)
}

fn parse_peer_urls(peer_urls: &[String]) -> Result<Vec<PeerConfig>, anyhow::Error> {
    let mut peers = vec![];
    for peer_url in peer_urls.iter() {
        let peer_url = peer_url.trim();
        if peer_url.is_empty() {
            continue;
        }
        peers.push(PeerConfig {
            uri: peer_url
                .parse()
                .with_context(|| format!("failed to parse peer uri: {}", peer_url))?,
            peer_public_key: None,
        });
    }
    Ok(peers)
}

impl NetworkConfigExt for NetworkConfig {
    #[allow(deprecated)]
    fn gen_config(&self) -> Result<TomlConfigLoader, anyhow::Error> {
        let cfg = TomlConfigLoader::default();
        cfg.set_id(
            self.instance_id
                .clone()
                .unwrap_or(uuid::Uuid::new_v4().to_string())
                .parse()
                .with_context(|| format!("failed to parse instance id: {:?}", self.instance_id))?,
        );
        cfg.set_hostname(self.hostname.clone());
        cfg.set_sni(self.sni.clone());
        super::toml::validate_transport_tuning(
            self.parallel_connections,
            self.traffic_disguise.as_deref(),
        )?;
        cfg.set_parallel_connections(self.parallel_connections);
        cfg.set_disable_parallel_connections(self.disable_parallel_connections);
        cfg.set_traffic_disguise(self.traffic_disguise.clone());
        cfg.set_dhcp(self.dhcp.unwrap_or(self.dhcp_ipv4_cidr.is_some()));
        if cfg.get_dhcp()
            && let Some(cidr) = &self.dhcp_ipv4_cidr
        {
            cfg.set_dhcp_config(super::DhcpConfig::from_cidr(cidr)?);
        }
        cfg.set_dhcpv6(self.dhcpv6.unwrap_or(self.dhcp_ipv6_cidr.is_some()));
        if cfg.get_dhcpv6()
            && let Some(cidr) = &self.dhcp_ipv6_cidr
        {
            cfg.set_dhcpv6_config(super::DhcpConfig::from_cidr(cidr)?);
        }
        cfg.set_inst_name(self.network_name.clone().unwrap_or_default());

        // The web UI does not expose credential inputs directly, but imported/saved
        // NetworkConfig objects still need to preserve credential-mode instances via
        // secure_mode.local_private_key + empty network_secret.
        let credential_secret = if self.network_secret.is_some() {
            None
        } else {
            self.secure_mode
                .as_ref()
                .and_then(|mode| mode.local_private_key.clone())
                .filter(|s| !s.is_empty())
        };

        if credential_secret.is_some() {
            cfg.set_network_identity(NetworkIdentity::new_credential(
                self.network_name.clone().unwrap_or_default(),
            ));
        } else {
            cfg.set_network_identity(NetworkIdentity::new(
                self.network_name.clone().unwrap_or_default(),
                self.network_secret.clone().unwrap_or_default(),
            ));
        }

        if !cfg.get_dhcp() {
            let virtual_ipv4 = self.virtual_ipv4.clone().unwrap_or_default();
            if !virtual_ipv4.is_empty() {
                let ip = format!("{}/{}", virtual_ipv4, self.network_length.unwrap_or(24))
                    .parse()
                    .with_context(|| {
                        format!(
                            "failed to parse ipv4 inet address: {}, {:?}",
                            virtual_ipv4, self.network_length
                        )
                    })?;
                cfg.set_ipv4(Some(ip));
            }
        }

        match NetworkingMethod::try_from(self.networking_method.unwrap_or_default())
            .unwrap_or_default()
        {
            NetworkingMethod::PublicServer => {
                let peers = parse_peers(&self.peers)?;
                if peers.is_empty() {
                    let public_server_url = self.public_server_url.clone().unwrap_or_default();
                    cfg.set_peers(vec![PeerConfig {
                        uri: public_server_url.parse().with_context(|| {
                            format!("failed to parse public server uri: {}", public_server_url)
                        })?,
                        peer_public_key: None,
                    }]);
                } else {
                    cfg.set_peers(peers);
                }
            }
            NetworkingMethod::Manual => {
                let mut peers = parse_peers(&self.peers)?;
                if peers.is_empty() {
                    peers = parse_peer_urls(&self.peer_urls)?;
                }
                if !peers.is_empty() {
                    cfg.set_peers(peers);
                }
            }
            NetworkingMethod::Standalone => {}
        }

        let mut listener_urls = vec![];
        for listener_url in self.listener_urls.iter() {
            if listener_url.is_empty() {
                continue;
            }
            listener_urls.push(
                listener_url
                    .parse()
                    .with_context(|| format!("failed to parse listener uri: {}", listener_url))?,
            );
        }
        cfg.set_listeners(listener_urls);

        for n in self.proxy_cidrs.iter() {
            add_proxy_network_to_config(n, &cfg)?;
        }

        if !self.port_forwards.is_empty() {
            let mut forwards = Vec::new();
            for (index, pf) in self.port_forwards.iter().enumerate() {
                // A fully blank row is an untouched form entry, not a rule.
                if pf.bind_ip.is_empty()
                    && pf.dst_ip.is_empty()
                    && pf.bind_port == 0
                    && pf.dst_port == 0
                {
                    continue;
                }
                let rule = |detail: String| {
                    format!(
                        "port forward #{} ({}:{} -> {}:{}): {}",
                        index, pf.bind_ip, pf.bind_port, pf.dst_ip, pf.dst_port, detail
                    )
                };
                let parse_ip = |field: &str, ip: &str| -> anyhow::Result<std::net::IpAddr> {
                    ip.parse().with_context(|| {
                        rule(format!("{field} address {ip:?} is not a valid IP address"))
                    })
                };
                let parse_port = |field: &str, port: u32| -> anyhow::Result<u16> {
                    // Port 0 stays legal: it binds an OS-assigned port.
                    u16::try_from(port).with_context(|| {
                        rule(format!("{field} port {port} must be between 0 and 65535"))
                    })
                };
                let proto = pf.proto.to_lowercase();
                if !matches!(proto.as_str(), "tcp" | "udp") {
                    anyhow::bail!(rule(format!("protocol {:?} must be tcp or udp", pf.proto)));
                }
                forwards.push(PortForwardConfig {
                    bind_addr: SocketAddr::new(
                        parse_ip("bind", &pf.bind_ip)?,
                        parse_port("bind", pf.bind_port)?,
                    ),
                    dst_addr: SocketAddr::new(
                        parse_ip("destination", &pf.dst_ip)?,
                        parse_port("destination", pf.dst_port)?,
                    ),
                    proto,
                });
            }
            cfg.set_port_forwards(forwards);
        }

        if self.enable_vpn_portal == Some(true) {
            anyhow::bail!(
                "legacy VPN portal configuration is no longer supported; configure vpn_portal_config with named clients"
            );
        }

        if let Some(vpn_config) = &self.vpn_portal_config {
            cfg.set_vpn_portal_config(VpnPortalConfig {
                enabled: vpn_config.enabled,
                wireguard_listen: vpn_config.wireguard_listen.parse().with_context(|| {
                    format!(
                        "failed to parse vpn portal wireguard listen address: {}",
                        vpn_config.wireguard_listen
                    )
                })?,
                wireguard_private_key: vpn_config.wireguard_private_key.clone(),
                clients: vpn_config
                    .clients
                    .iter()
                    .map(|client| {
                        Ok(VpnPortalClientConfig {
                            name: client.name.clone(),
                            virtual_ip: client.virtual_ip.parse().with_context(|| {
                                format!(
                                    "failed to parse vpn portal virtual IP for client {}: {}",
                                    client.name, client.virtual_ip
                                )
                            })?,
                            groups: client.groups.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, anyhow::Error>>()?,
            });
        }

        if self.enable_manual_routes.unwrap_or_default() {
            let mut routes = Vec::<cidr::Ipv4Cidr>::with_capacity(self.routes.len());
            for route in self.routes.iter() {
                routes.push(
                    route
                        .parse()
                        .with_context(|| format!("failed to parse route: {}", route))?,
                );
            }
            cfg.set_routes(Some(routes));
        }

        if let Some(vpn) = self.vpn.as_ref() {
            let vpn = crate::config::vpn::pb::from_pb(vpn.clone())
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            cfg.set_vpn(vpn);
        }

        if self.enable_socks5.unwrap_or_default()
            && let Some(socks5_port) = self.socks5_port
        {
            cfg.set_socks5_portal(Some(
                format!("socks5://0.0.0.0:{}", socks5_port).parse().unwrap(),
            ));
        }

        if !self.mapped_listeners.is_empty() {
            let mapped_listeners = parse_mapped_listener_urls(&self.mapped_listeners)?;
            cfg.set_mapped_listeners(Some(mapped_listeners));
        }

        if let Some(credential_file) = self
            .credential_file
            .as_ref()
            .filter(|path| !path.is_empty())
        {
            cfg.set_credential_file(Some(credential_file.into()));
        }

        cfg.set_managed_credentials(
            self.managed_credentials
                .iter()
                .map(|credential| credential.clone().upgrade())
                .collect::<Result<Vec<_>, _>>()?,
        );

        if let Some(credential_secret) = credential_secret {
            cfg.set_secure_mode(Some(normalize_secure_mode_config(
                easytier_proto::common::SecureModeConfig {
                    enabled: true,
                    local_private_key: Some(credential_secret),
                    local_public_key: None,
                },
            )?));
        } else {
            cfg.set_secure_mode(
                self.secure_mode
                    .clone()
                    .map(normalize_secure_mode_config)
                    .transpose()?,
            );
        }

        if let Some(acl) = self.acl.as_ref()
            && !acl.is_empty()
        {
            cfg.set_acl(Some(acl.clone()));
        }

        cfg.patch_flags(FlagsPatch {
            default_protocol: self.p2p_prefer_protocol.as_deref().map(|protocol| {
                let protocol = protocol.trim();
                if protocol.is_empty() {
                    super::DEFAULT_PROTOCOL.to_owned()
                } else {
                    protocol.to_owned()
                }
            }),
            dev_name: self.dev_name.clone(),
            enable_ipv6: self.disable_ipv6.map(|disabled| !disabled),
            socket_mark: self.socket_mark,
            enable_encryption: self.disable_encryption.map(|disabled| !disabled),
            relay_network_whitelist: self.enable_relay_network_whitelist.map(|enabled| {
                if enabled {
                    self.relay_network_whitelist.join(" ")
                } else {
                    "*".to_owned()
                }
            }),
            mtu: self
                .mtu
                .map(u32::try_from)
                .transpose()
                .context("invalid mtu: expected a non-negative integer")?,
            private_mode: self.enable_private_mode,
            encryption_algorithm: self.encryption_algorithm.clone(),
            data_compress_algo: self.data_compress_algo.map(|algorithm| algorithm.max(1)),
            latency_first: self.latency_first,
            use_smoltcp: self.use_smoltcp,
            enable_kcp_proxy: self.enable_kcp_proxy,
            disable_kcp_input: self.disable_kcp_input,
            enable_quic_proxy: self.enable_quic_proxy,
            disable_quic_input: self.disable_quic_input,
            disable_p2p: self.disable_p2p,
            p2p_only: self.p2p_only,
            lazy_p2p: self.lazy_p2p,
            bind_device: self.bind_device,
            no_tun: self.no_tun,
            relay_all_peer_rpc: self.relay_all_peer_rpc,
            need_p2p: self.need_p2p,
            multi_thread: self.multi_thread,
            proxy_forward_by_system: self.proxy_forward_by_system,
            disable_tcp_hole_punching: self.disable_tcp_hole_punching,
            disable_udp_hole_punching: self.disable_udp_hole_punching,
            disable_upnp: self.disable_upnp,
            disable_relay_data: self.disable_relay_data,
            prefer_peer_relay: self.prefer_peer_relay,
            enable_udp_broadcast_relay: self.enable_udp_broadcast_relay,
            disable_sym_hole_punching: self.disable_sym_hole_punching,
            p2p_disguise_mode: self.p2p_disguise_mode.clone(),
            close_redundant_conns_when_disguised: self.close_redundant_conns_when_disguised,
            instance_recv_bps_limit: self.instance_recv_bps_limit,
            ..Default::default()
        });
        cfg.get_flags_patch().validate_p2p_disguise()?;
        cfg.prune_form_defaults();
        Ok(cfg)
    }

    fn new_from_config(config: impl ConfigLoader) -> Result<Self, anyhow::Error> {
        let default_config = TomlConfigLoader::default();

        let mut result = Self {
            ..Default::default()
        };

        result.instance_id = Some(config.get_id().to_string());
        if config.get_hostname() != default_config.get_hostname() {
            result.hostname = Some(config.get_hostname());
        }
        if config.get_sni() != default_config.get_sni() {
            result.sni = Some(config.get_sni());
        }
        if config.get_parallel_connections() != 1 {
            result.parallel_connections = Some(config.get_parallel_connections());
        }
        if config.get_disable_parallel_connections() {
            result.disable_parallel_connections = Some(true);
        }
        if config.get_traffic_disguise() != "speedtest" {
            result.traffic_disguise = Some(config.get_traffic_disguise());
        }

        result.dhcp = Some(config.get_dhcp());
        result.dhcp_ipv4_cidr = config.get_dhcp_config().cidr().map(|cidr| cidr.to_string());
        result.dhcpv6 = Some(config.get_dhcpv6());
        result.dhcp_ipv6_cidr = config
            .get_dhcpv6_config()
            .cidr()
            .map(|cidr| cidr.to_string());

        let network_identity = config.get_network_identity();
        result.network_name = Some(network_identity.network_name.clone());
        result.network_secret = network_identity.network_secret;

        if let Some(ipv4) = config.get_ipv4() {
            result.virtual_ipv4 = Some(ipv4.address().to_string());
            result.network_length = Some(ipv4.network_length() as i32);
        }

        let vpn = config.get_vpn();
        if vpn != default_config.get_vpn() {
            result.vpn = Some(crate::config::vpn::pb::to_pb(&vpn));
        }

        let peers = config.get_peers();
        result.networking_method = Some(NetworkingMethod::Manual as i32);
        if !peers.is_empty() {
            result.peer_urls = peers.iter().map(|p| p.uri.to_string()).collect();
            result.peers = peers
                .iter()
                .map(|p| manage::NetworkPeerConfig {
                    uri: p.uri.to_string(),
                    peer_public_key: p.peer_public_key.clone(),
                })
                .collect();
        }

        result.listener_urls = config
            .get_listeners()
            .unwrap_or_default()
            .iter()
            .map(|l| l.to_string())
            .collect();

        result.proxy_cidrs = config
            .get_proxy_cidrs()
            .iter()
            .map(|c| {
                if let Some(mapped) = c.mapped_cidr {
                    format!("{}->{}", c.cidr, mapped)
                } else {
                    c.cidr.to_string()
                }
            })
            .collect();

        let port_forwards = config.get_port_forwards();
        if !port_forwards.is_empty() {
            result.port_forwards = port_forwards
                .iter()
                .map(|f| manage::PortForwardConfig {
                    proto: f.proto.clone(),
                    bind_ip: f.bind_addr.ip().to_string(),
                    bind_port: f.bind_addr.port() as u32,
                    dst_ip: f.dst_addr.ip().to_string(),
                    dst_port: f.dst_addr.port() as u32,
                })
                .collect();
        }

        if let Some(vpn_config) = config.get_vpn_portal_config() {
            result.vpn_portal_config = Some(manage::VpnPortalConfig {
                enabled: vpn_config.enabled,
                wireguard_listen: vpn_config.wireguard_listen.to_string(),
                wireguard_private_key: vpn_config.wireguard_private_key,
                clients: vpn_config
                    .clients
                    .into_iter()
                    .map(|client| manage::VpnPortalClientConfig {
                        name: client.name,
                        virtual_ip: client.virtual_ip.to_string(),
                        groups: client.groups,
                    })
                    .collect(),
            });
        }

        if let Some(routes) = config.get_routes() {
            result.enable_manual_routes = Some(true);
            result.routes = routes.iter().map(|r| r.to_string()).collect();
        }

        if let Some(socks5_portal) = config.get_socks5_portal() {
            result.enable_socks5 = Some(true);
            result.socks5_port = socks5_portal.port().map(|p| p as i32);
        }

        let mapped_listeners = config.get_mapped_listeners();
        if !mapped_listeners.is_empty() {
            result.mapped_listeners = mapped_listeners.iter().map(|l| l.to_string()).collect();
        }

        result.secure_mode = config.get_secure_mode();
        result.credential_file = config
            .get_credential_file()
            .map(|path| path.to_string_lossy().into_owned());
        result.managed_credentials = config
            .get_managed_credentials()
            .into_iter()
            .map(|credential| credential.downgrade())
            .collect();
        set_network_flags(&mut result, config.get_flags_patch());
        result.acl = config.get_acl();

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]

    use super::*;
    use easytier_proto::common::Flags;

    fn api_portal_config() -> manage::VpnPortalConfig {
        manage::VpnPortalConfig {
            enabled: None,
            wireguard_listen: "0.0.0.0:51820".to_owned(),
            wireguard_private_key: Some("server-private-key".to_owned()),
            clients: vec![manage::VpnPortalClientConfig {
                name: "alice".to_owned(),
                virtual_ip: "10.144.144.10/16".to_owned(),
                groups: vec!["staff".to_owned()],
            }],
        }
    }

    fn standalone_config() -> NetworkConfig {
        NetworkConfig {
            networking_method: Some(NetworkingMethod::Standalone as i32),
            ..Default::default()
        }
    }

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

    #[test]
    fn removed_dns_fields_are_reserved_in_wire_schema() {
        let pool =
            prost_reflect::DescriptorPool::decode(easytier_proto::ALL_DESCRIPTOR_BYTES).unwrap();
        for (message, number, name) in [
            ("common.Flags", 22, "accept_dns"),
            ("common.Flags", 31, "tld_dns_zone"),
            ("api.manage.NetworkConfig", 42, "enable_magic_dns"),
        ] {
            let descriptor = pool.get_message_by_name(message).unwrap();
            assert!(descriptor.get_field(number).is_none());
            assert!(descriptor.get_field_by_name(name).is_none());
            let schema = descriptor.descriptor_proto();
            assert!(schema.reserved_name.iter().any(|reserved| reserved == name));
            assert!(
                schema.reserved_range.iter().any(|range| {
                    range.start() <= number as i32 && (number as i32) < range.end()
                })
            );
        }
        assert!(
            pool.get_service_by_name("magic_dns.MagicDnsServerRpc")
                .is_none()
        );
    }

    /// The management API's name for a flag is the schema's to state, and the
    /// projection is what actually names the fields. Turning the flag the
    /// annotation names must turn the field it points at, in the direction the
    /// annotation declares.
    #[test]
    fn declared_api_names_match_the_projection() {
        use prost_reflect::{DescriptorPool, Value};

        let pool = DescriptorPool::decode(easytier_proto::ALL_DESCRIPTOR_BYTES).unwrap();
        let flags = pool.get_message_by_name("common.Flags").unwrap();
        let extension = pool.get_extension_by_name("easytier.flag").unwrap();

        // A patch states every field, so the values survive serialization:
        // protobuf JSON omits a field holding its zero value.
        let patch: FlagsPatch = Flags::defaults().downgrade();
        let defaults = serde_json::to_value(&patch).unwrap();

        for field in flags.fields() {
            let name = field.name();
            let options = field.options();
            let annotation = options.get_extension(&extension).into_owned();
            let Value::Message(meta) = annotation else {
                panic!("{name}: the annotation is not a message");
            };
            if !meta.has_field_by_name("api") {
                continue;
            }
            let Value::Message(spelling) = meta.get_field_by_name("api").unwrap().into_owned()
            else {
                panic!("{name}: the api spelling is not a message");
            };
            let api_field = match spelling.get_field_by_name("field").as_deref() {
                Some(Value::String(api_field)) if !api_field.is_empty() => api_field.clone(),
                _ => continue,
            };
            let negate = matches!(
                spelling.get_field_by_name("negate").as_deref(),
                Some(Value::Bool(true))
            );

            let (changed, expected) = match &defaults[name] {
                serde_json::Value::Bool(declared) => (
                    serde_json::json!(!declared),
                    serde_json::json!(if negate { *declared } else { !declared }),
                ),
                serde_json::Value::String(_) => {
                    assert!(!negate, "{name}: only boolean flags can be negated");
                    (serde_json::json!("tcp"), serde_json::json!("tcp"))
                }
                value => panic!("{name}: unexpected API-mapped flag value {value:?}"),
            };

            let mut flipped = defaults.clone();
            flipped[name] = changed;
            let patch: FlagsPatch = serde_json::from_value(flipped).unwrap();

            let mut projected = NetworkConfig::default();
            set_network_flags(&mut projected, patch);

            // Protobuf JSON omits a field holding its zero value.
            let projected = serde_json::to_value(&projected).unwrap();
            let value = projected
                .get(&api_field)
                .cloned()
                .unwrap_or(serde_json::json!(false));
            assert_eq!(
                value, expected,
                "flipping {name} should leave {api_field} as the annotation declares"
            );
        }
    }

    #[cfg(feature = "config-write")]
    #[test]
    fn web_form_omits_defaults_without_changing_effective_flags() {
        let mut input = standalone_config();
        set_network_flags(&mut input, Flags::defaults().downgrade());
        input.vpn = Some(manage::VpnConfig {
            exit_enabled: true,
            rules: vec![],
        });
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
        assert!(restored.get_vpn().exit_enabled);

        input.enable_private_mode = Some(false);
        input.disable_encryption = Some(true);
        input.socket_mark = Some(0);
        input.instance_recv_bps_limit = Some(0);
        input.vpn.as_mut().unwrap().exit_enabled = false;
        let generated = input.gen_config().unwrap();
        let document: toml::Table = toml::from_str(&generated.dump()).unwrap();
        assert_eq!(document["flags"]["private_mode"].as_bool(), Some(false));
        assert_eq!(
            document["flags"]["enable_encryption"].as_bool(),
            Some(false)
        );
        assert_eq!(document["flags"]["socket_mark"].as_integer(), Some(0));
        assert_eq!(document["vpn"]["exit_enabled"].as_bool(), Some(false));
        assert!(!generated.get_vpn().exit_enabled);
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
    fn api_flags_round_trip_preserves_missing_default_and_custom_values() {
        for disabled in [None, Some(false), Some(true)] {
            for whitelist in [None, Some(false), Some(true)] {
                let input = NetworkConfig {
                    disable_encryption: disabled,
                    enable_relay_network_whitelist: whitelist,
                    latency_first: Some(false),
                    mtu: Some(0),
                    instance_recv_bps_limit: Some(u64::MAX),
                    ..standalone_config()
                };
                let config = input.gen_config().unwrap();
                let config = TomlConfigLoader::new_from_str(&config.dump()).unwrap();
                let output = NetworkConfig::new_from_config(&config).unwrap();
                assert_eq!(output.disable_encryption, disabled.filter(|value| *value));
                assert_eq!(
                    output.enable_relay_network_whitelist,
                    whitelist.filter(|value| *value)
                );
                assert_eq!(output.latency_first, None);
                assert_eq!(output.mtu, Some(0));
                assert_eq!(output.instance_recv_bps_limit, None);
                assert_eq!(output.disable_ipv6, None);
                assert_eq!(output.encryption_algorithm, None);
            }
        }
        assert!(
            NetworkConfig {
                mtu: Some(-1),
                ..standalone_config()
            }
            .gen_config()
            .is_err()
        );
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
    fn vpn_portal_api_config_round_trips_through_toml_model() {
        let input = NetworkConfig {
            vpn_portal_config: Some(api_portal_config()),
            ..standalone_config()
        };

        let config = input.gen_config().unwrap();
        let portal = config.get_vpn_portal_config().unwrap();
        assert_eq!(portal.wireguard_listen, "0.0.0.0:51820".parse().unwrap());
        assert_eq!(
            portal.wireguard_private_key.as_deref(),
            Some("server-private-key")
        );
        assert_eq!(portal.clients[0].name, "alice");
        assert_eq!(portal.clients[0].virtual_ip.to_string(), "10.144.144.10/16");
        assert_eq!(portal.clients[0].groups, vec!["staff".to_owned()]);

        let output = NetworkConfig::new_from_config(&config).unwrap();
        assert_eq!(output.vpn_portal_config, input.vpn_portal_config);
        assert_eq!(output.enable_vpn_portal, None);
    }

    #[test]
    fn disabled_vpn_portal_preserves_clients_and_key_without_a_runtime() {
        let mut portal = api_portal_config();
        portal.enabled = Some(false);
        let input = NetworkConfig {
            vpn_portal_config: Some(portal),
            ..standalone_config()
        };
        let config = input.gen_config().unwrap();
        let restored = crate::config::toml::TomlConfig::new_from_str(&config.dump()).unwrap();
        let output = NetworkConfig::new_from_config(&restored).unwrap();
        assert_eq!(output.vpn_portal_config, input.vpn_portal_config);
        assert!(
            crate::instance::CoreInstanceConfig::from_toml(&restored)
                .unwrap()
                .vpn_portal
                .is_none()
        );

        let mut portal = restored.get_vpn_portal_config().unwrap();
        portal.enabled = Some(true);
        restored.set_vpn_portal_config(portal);
        let runtime = crate::instance::CoreInstanceConfig::from_toml(&restored).unwrap();
        assert_eq!(runtime.vpn_portal.unwrap().clients[0].name, "alice");
    }

    #[test]
    fn managed_credentials_round_trip_through_toml_model() {
        let input = NetworkConfig {
            managed_credentials: vec![manage::ManagedCredentialConfig {
                credential_id: "managed-a".to_owned(),
                credential_secret: "secret".to_owned(),
                groups: vec!["ops".to_owned()],
                allow_relay: true,
                allowed_proxy_cidrs: vec!["10.0.0.0/24".to_owned()],
                expiry_unix: 2_000_000_000,
                reusable: None,
            }],
            ..standalone_config()
        };

        let config = input.gen_config().unwrap();
        let output = NetworkConfig::new_from_config(&config).unwrap();
        assert_eq!(output.managed_credentials[0].credential_id, "managed-a");
        assert_eq!(output.managed_credentials[0].reusable, Some(true));
    }

    #[test]
    fn peer_relay_preference_round_trips_independently() {
        let input = NetworkConfig {
            disable_relay_data: Some(false),
            prefer_peer_relay: Some(true),
            ..standalone_config()
        };

        let config = input.gen_config().unwrap();
        let flags = config.get_flags();
        assert!(!flags.disable_relay_data);
        assert!(flags.prefer_peer_relay);

        let output = NetworkConfig::new_from_config(&config).unwrap();
        assert_eq!(output.disable_relay_data, None);
        assert_eq!(output.prefer_peer_relay, Some(true));
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

    #[test]
    fn legacy_enabled_vpn_portal_config_reports_migration_error() {
        let error = NetworkConfig {
            enable_vpn_portal: Some(true),
            ..standalone_config()
        }
        .gen_config()
        .unwrap_err()
        .to_string();

        assert!(error.contains("legacy VPN portal"), "{error}");
    }

    #[test]
    fn legacy_disabled_vpn_portal_defaults_are_ignored() {
        let config = NetworkConfig {
            enable_vpn_portal: Some(false),
            vpn_portal_listen_port: Some(0),
            vpn_portal_client_network_addr: Some(String::new()),
            vpn_portal_client_network_len: Some(0),
            ..standalone_config()
        }
        .gen_config()
        .unwrap();

        assert!(config.get_vpn_portal_config().is_none());
    }

    #[cfg(feature = "browser-config")]
    #[test]
    fn browser_merge_reconciles_renamed_flags() {
        let original = r#"
[flags]
default_protocol = "udp"
enable_encryption = false
enable_ipv6 = false
private_mode = false
"#;

        for value in [Some(false), Some(true), None] {
            let network_config = NetworkConfig {
                disable_encryption: value,
                disable_ipv6: value,
                enable_private_mode: value,
                p2p_prefer_protocol: Some("udp".to_owned()),
                ..standalone_config()
            };
            let merged = merge_network_config_toml(original, &network_config).unwrap();
            let merged: toml::Table = toml::from_str(&merged).unwrap();
            let flags = merged
                .get("flags")
                .and_then(toml::Value::as_table)
                .cloned()
                .unwrap_or_default();

            assert!(!flags.contains_key("default_protocol"));
            for (field, expected) in [
                (
                    "enable_encryption",
                    value.filter(|disabled| *disabled).map(|disabled| !disabled),
                ),
                (
                    "enable_ipv6",
                    value.filter(|disabled| *disabled).map(|disabled| !disabled),
                ),
                ("private_mode", value.filter(|enabled| !enabled)),
            ] {
                assert_eq!(
                    flags.get(field).and_then(toml::Value::as_bool),
                    expected,
                    "merging {field} from API value {value:?}"
                );
            }
        }
    }

    #[cfg(feature = "browser-config")]
    #[test]
    fn browser_merge_preserves_fields_outside_the_shared_form() {
        let original = r#"
instance_name = "module-instance"
rpc_portal = "0.0.0.0:15888"
tcp_whitelist = ["22"]
stun_servers = ["custom.example.com:3478"]

[network_identity]
network_name = "old-network"
network_secret = "secret"

[flags]
default_protocol = "tcp"
disable_p2p = true
"#;
        let parsed = TomlConfigLoader::new_from_str(original).unwrap();
        let mut network_config = NetworkConfig::new_from_config(&parsed).unwrap();
        network_config.network_name = Some("edited-network".to_owned());
        network_config.disable_p2p = Some(false);

        let merged = merge_network_config_toml(original, &network_config).unwrap();
        let merged: toml::Table = toml::from_str(&merged).unwrap();

        assert_eq!(merged["instance_name"].as_str(), Some("module-instance"));
        assert_eq!(merged["rpc_portal"].as_str(), Some("0.0.0.0:15888"));
        assert_eq!(merged["tcp_whitelist"][0].as_str(), Some("22"));
        assert_eq!(
            merged["stun_servers"][0].as_str(),
            Some("custom.example.com:3478")
        );
        assert_eq!(
            merged["network_identity"]["network_name"].as_str(),
            Some("edited-network")
        );
        assert_eq!(merged["flags"]["default_protocol"].as_str(), Some("tcp"));
        assert!(merged["flags"].get("disable_p2p").is_none());
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

    #[cfg(feature = "browser-config")]
    #[test]
    fn browser_merge_normalizes_camel_case_flag_aliases() {
        let original = r#"
[flags]
disableP2p = true
enableEncryption = false
foreignRelayBpsLimit = 1234
"#;
        let parsed_orig = TomlConfigLoader::new_from_str(original).unwrap();
        let mut network_config = NetworkConfig::new_from_config(&parsed_orig).unwrap();
        network_config.disable_p2p = Some(false);

        let merged = merge_network_config_toml(original, &network_config).unwrap();
        let parsed = TomlConfigLoader::new_from_str(&merged)
            .expect("merged config should be parseable without duplicate-field error");
        assert!(!parsed.get_flags().disable_p2p);
        assert!(!parsed.get_flags().enable_encryption);
        assert_eq!(parsed.get_flags().foreign_relay_bps_limit, 1234);

        let merged_table: toml::Table = toml::from_str(&merged).unwrap();
        let flags = merged_table["flags"].as_table().unwrap();
        assert_eq!(
            flags.get("disable_p2p").and_then(toml::Value::as_bool),
            None
        );
        assert!(flags.get("disableP2p").is_none());
        assert_eq!(
            flags
                .get("enable_encryption")
                .and_then(toml::Value::as_bool),
            Some(false)
        );
        assert!(flags.get("enableEncryption").is_none());
        assert_eq!(
            flags
                .get("foreign_relay_bps_limit")
                .and_then(toml::Value::as_integer),
            Some(1234)
        );
        assert!(flags.get("foreignRelayBpsLimit").is_none());
    }

    #[cfg(feature = "browser-config")]
    #[test]
    fn browser_merge_repro_camel_case_disable_p2p_collision() {
        let original = r#"
[flags]
disableP2p = false
"#;
        let mut network_config = standalone_config();
        network_config.hostname = Some("new-hostname".to_owned());
        network_config.disable_p2p = Some(false);

        let merged = merge_network_config_toml(original, &network_config).unwrap();
        let parsed = TomlConfigLoader::new_from_str(&merged)
            .expect("saving config with camelCase alias must not fail on reload");
        assert!(!parsed.get_flags().disable_p2p);

        let merged_table: toml::Table = toml::from_str(&merged).unwrap();
        assert!(!merged_table.contains_key("flags"));
    }
    #[test]
    fn manual_routes_preserve_empty_and_automatic_modes() {
        for routes in [Vec::new(), vec!["10.20.0.0/24".to_owned()]] {
            for enabled in [None, Some(false), Some(true)] {
                let mut input = standalone_config();
                input.enable_manual_routes = enabled;
                input.routes = routes.clone();
                let config = input.gen_config().unwrap();
                let expected = if enabled == Some(true) {
                    Some(routes.iter().map(|route| route.parse().unwrap()).collect())
                } else {
                    None
                };
                assert_eq!(config.get_routes(), expected);
                let readback = NetworkConfig::new_from_config(config.clone()).unwrap();
                assert_eq!(
                    readback.enable_manual_routes,
                    enabled.filter(|value| *value)
                );
                assert_eq!(readback.gen_config().unwrap().get_routes(), expected);
                assert_eq!(
                    TomlConfigLoader::new_from_str(&config.dump())
                        .unwrap()
                        .get_routes(),
                    expected
                );
                #[cfg(feature = "web-client")]
                {
                    let projected = super::super::api::network_config_from_toml(&config);
                    assert_eq!(
                        projected.enable_manual_routes,
                        readback.enable_manual_routes
                    );
                    assert_eq!(projected.routes, readback.routes);
                }
            }
        }
    }

    #[test]
    fn dhcp_cidr_survives_api_and_toml_round_trips() {
        let mut input = standalone_config();
        input.dhcp_ipv4_cidr = Some("192.168.100.0/24".to_owned());
        for enabled in [None, Some(true), Some(false)] {
            input.dhcp = enabled;
            let config = input.gen_config().unwrap();
            assert_eq!(config.get_dhcp(), enabled.unwrap_or(true));
            let readback = NetworkConfig::new_from_config(config.clone()).unwrap();
            let expected_cidr = if enabled == Some(false) {
                None
            } else {
                input.dhcp_ipv4_cidr.clone()
            };
            assert_eq!(readback.dhcp_ipv4_cidr, expected_cidr);
            assert_eq!(
                TomlConfigLoader::new_from_str(&config.dump())
                    .unwrap()
                    .get_dhcp_config(),
                readback.gen_config().unwrap().get_dhcp_config()
            );
            #[cfg(feature = "web-client")]
            assert_eq!(
                super::super::api::network_config_from_toml(&config).dhcp_ipv4_cidr,
                expected_cidr
            );
        }
    }

    #[test]
    fn dhcpv6_cidr_survives_management_round_trips() {
        let mut input = NetworkConfig {
            dhcp: Some(true),
            dhcp_ipv6_cidr: Some("fd12::/64".to_owned()),
            ..standalone_config()
        };
        for enabled in [None, Some(true), Some(false)] {
            input.dhcpv6 = enabled;
            let config = input.gen_config().unwrap();
            assert_eq!(config.get_dhcpv6(), enabled.unwrap_or(true));
            let readback = NetworkConfig::new_from_config(&config).unwrap();
            assert_eq!(readback.dhcpv6, Some(enabled.unwrap_or(true)));
            assert_eq!(
                readback.dhcp_ipv6_cidr,
                if enabled == Some(false) {
                    None
                } else {
                    input.dhcp_ipv6_cidr.clone()
                }
            );
            assert_eq!(
                readback.gen_config().unwrap().get_dhcpv6_config(),
                config.get_dhcpv6_config()
            );
            assert_eq!(
                super::super::api::network_config_from_toml(&config).dhcp_ipv6_cidr,
                readback.dhcp_ipv6_cidr
            );
        }
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

    #[test]
    fn dhcp_api_rejects_invalid_or_unusable_subnets() {
        let mut input = standalone_config();
        for value in [
            "true",
            "false",
            "yes",
            "::/64",
            "192.168.100.0/31",
            "192.168.100.1/32",
        ] {
            input.dhcp_ipv4_cidr = Some(value.to_owned());
            assert!(input.gen_config().is_err(), "accepted {value}");
        }
    }
}
