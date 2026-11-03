use std::{
    collections::{HashMap, HashSet},
    net::Ipv4Addr,
};

use easytier::{
    common::config::{NetworkConfig, NetworkConfigExt, NetworkingMethod},
    proto::{api::manage::ManagedCredentialConfig, common::SecureModeConfig},
};
use easytier_core::{
    config::runtime::CoreInstanceRuntimeConfig, connectivity::manual::validate_manual_url,
    gateway::vpn_portal::validate_clients, instance::CoreInstanceConfig,
    peers::credential_manager::validate_managed_credential_set,
};
use uuid::Uuid;

use super::{
    model::{
        CentralNetworkIntent, CredentialGrant, NetworkCredentialIntent, NetworkMemberIntent,
        NetworkMode, member_group_name,
    },
    network_acl::{self, NetworkAcl},
};

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledNetwork {
    pub members: Vec<CompiledMember>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledMember {
    pub member_id: Uuid,
    pub device_id: String,
    pub network_config: NetworkConfig,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CompileError {
    #[error("display name must not be empty")]
    EmptyDisplayName,
    #[error("network name must not be empty")]
    EmptyNetworkName,
    #[error("invalid network URL: {0}")]
    InvalidUrl(String),
    #[error("manual networking requires at least one peer URL")]
    EmptyPeerUrls,
    #[error("duplicate device in network intent: {0}")]
    DuplicateDevice(String),
    #[error("invalid CIDR: {0}")]
    InvalidCidr(String),
    #[error("invalid virtual subnet: {0}")]
    InvalidVirtualSubnet(String),
    #[error("invalid IPv4 address for member {member_id}: {address}")]
    InvalidMemberIp { member_id: Uuid, address: String },
    #[error("duplicate virtual IPv4 address: {0}")]
    DuplicateMemberIp(Ipv4Addr),
    #[error("virtual subnet has no free host address for member {0}")]
    SubnetExhausted(Uuid),
    #[error("invalid credential for member {member_id}: {reason}")]
    InvalidMemberCredential { member_id: Uuid, reason: String },
    #[error("invalid configuration for member {member_id}: {reason}")]
    InvalidMemberConfig { member_id: Uuid, reason: String },
}

/// Compile a complete network intent without consulting external state.
///
/// Input order is deliberately normalized. Callers can therefore compare the
/// result directly when deciding whether materialized desired state changed.
pub fn compile(intent: &CentralNetworkIntent) -> Result<CompiledNetwork, CompileError> {
    compile_with_acl(intent, None)
}

/// Compile with the network's access control, which temporary members run
/// after their own ACL.
pub fn compile_with_acl(
    intent: &CentralNetworkIntent,
    network_acl: Option<&NetworkAcl>,
) -> Result<CompiledNetwork, CompileError> {
    validate_network(intent)?;

    let mut members: Vec<&NetworkMemberIntent> = intent.members.iter().collect();
    members.sort_by(|left, right| {
        left.device_id
            .cmp(&right.device_id)
            .then_with(|| left.id.cmp(&right.id))
    });
    let credentials = normalized_credentials(&intent.credentials)?;
    validate_members(intent, &members, &credentials)?;

    let addresses = assign_addresses(intent.virtual_cidr.as_deref(), &members)?;
    let (networking_method, public_server_url, peer_urls) = compile_mode(&intent.mode)?;
    let managed_credentials: Vec<ManagedCredentialConfig> = credentials
        .iter()
        .map(|credential| compile_managed_credential(credential))
        .collect();

    let compiled_members = members
        .into_iter()
        .map(|member| {
            let credential = member
                .credential_id
                .as_deref()
                .map(|credential_id| credentials_by_id(&credentials)[credential_id]);
            let temporary = credential.is_some();
            let address = addresses.get(&member.id).copied().flatten();
            let network_length = intent
                .virtual_cidr
                .as_deref()
                .map(parse_ipv4_cidr)
                .transpose()?
                .map(|subnet| i32::from(subnet.prefix))
                .or(Some(24));

            let base_config = NetworkConfig {
                instance_id: Some(intent.id.to_string()),
                dhcp: Some(address.is_none()),
                virtual_ipv4: address.map(|address| address.to_string()),
                network_length,
                hostname: member.hostname.clone(),
                network_name: Some(intent.network_name.clone()),
                network_secret: (!temporary).then(|| intent.network_secret.clone()),
                networking_method: Some(networking_method as i32),
                public_server_url: public_server_url.clone(),
                peer_urls: peer_urls.clone(),
                secure_mode: if temporary {
                    Some(SecureModeConfig {
                        enabled: true,
                        local_private_key: credential.map(|item| item.secret.clone()),
                        ..Default::default()
                    })
                } else {
                    intent.secure_mode.then(|| SecureModeConfig {
                        enabled: true,
                        ..Default::default()
                    })
                },
                managed_credentials: if intent.secure_mode && !temporary {
                    managed_credentials.clone()
                } else {
                    Vec::new()
                },
                disable_encryption: intent.disable_encryption.then_some(true),
                encryption_algorithm: intent.encryption_algorithm.clone(),
                instance_name: Some(intent.display_name.clone())
                    .filter(|name| !name.trim().is_empty()),
                ..Default::default()
            };
            let mut network_config =
                apply_member_override(base_config, member.config_override.as_ref());
            if let (Some(credential), Some(acl)) = (credential, network_acl) {
                let groups = credential.grant.acl_groups.iter().cloned().collect();
                network_acl::apply(
                    &mut network_config,
                    Some(&acl.for_credential_member(&groups)),
                );
            }
            validate_member_config(&network_config).map_err(|error| {
                CompileError::InvalidMemberConfig {
                    member_id: member.id,
                    reason: format!("{error:#}"),
                }
            })?;

            Ok(CompiledMember {
                member_id: member.id,
                device_id: member.device_id.clone(),
                network_config,
            })
        })
        .collect::<Result<Vec<_>, CompileError>>()?;

    Ok(CompiledNetwork {
        members: compiled_members,
    })
}

fn validate_member_config(config: &NetworkConfig) -> anyhow::Result<()> {
    let normalized = CoreInstanceConfig::from_toml(&config.gen_config()?)?;
    if let Some(portal) = &normalized.vpn_portal {
        validate_clients(
            portal,
            &CoreInstanceRuntimeConfig {
                services: normalized.connectivity.runtime,
                peer: normalized.peer.snapshot.into(),
            },
        )?;
    }
    Ok(())
}

fn apply_member_override(
    base: NetworkConfig,
    config_override: Option<&NetworkConfig>,
) -> NetworkConfig {
    let Some(config_override) = config_override else {
        return base;
    };
    let mut merged = config_override.clone();
    merged.instance_id = base.instance_id;
    merged.dhcp = base.dhcp;
    merged.dhcp_ipv4_cidr = base.dhcp_ipv4_cidr;
    merged.dhcpv6 = base.dhcpv6;
    merged.dhcp_ipv6_cidr = base.dhcp_ipv6_cidr;
    merged.virtual_ipv4 = base.virtual_ipv4;
    merged.network_length = base.network_length;
    merged.hostname = base.hostname;
    merged.network_name = base.network_name;
    merged.network_secret = base.network_secret;
    merged.networking_method = base.networking_method;
    merged.public_server_url = base.public_server_url;
    merged.peer_urls = base.peer_urls;
    merged.peers = base.peers;
    merged.secure_mode = base.secure_mode;
    merged.managed_credentials = base.managed_credentials;
    merged.credential_file = None;
    merged
}

fn validate_network(intent: &CentralNetworkIntent) -> Result<(), CompileError> {
    if intent.display_name.trim().is_empty() {
        return Err(CompileError::EmptyDisplayName);
    }
    if intent.network_name.trim().is_empty() {
        return Err(CompileError::EmptyNetworkName);
    }
    compile_mode(&intent.mode)?;
    Ok(())
}

fn compile_mode(
    mode: &NetworkMode,
) -> Result<(NetworkingMethod, Option<String>, Vec<String>), CompileError> {
    match mode {
        NetworkMode::PublicServer { url } => {
            validate_url(url)?;
            Ok((
                NetworkingMethod::PublicServer,
                Some(url.clone()),
                Vec::new(),
            ))
        }
        NetworkMode::Manual { peer_urls } => {
            if peer_urls.is_empty() {
                return Err(CompileError::EmptyPeerUrls);
            }
            for peer_url in peer_urls {
                validate_url(peer_url)?;
            }
            let mut peer_urls = peer_urls.clone();
            peer_urls.sort();
            peer_urls.dedup();
            Ok((NetworkingMethod::Manual, None, peer_urls))
        }
        NetworkMode::Standalone => Ok((NetworkingMethod::Standalone, None, Vec::new())),
        NetworkMode::Gateway { peer_url } => {
            validate_url(peer_url)?;
            Ok((NetworkingMethod::Manual, None, vec![peer_url.clone()]))
        }
    }
}

fn validate_url(value: &str) -> Result<(), CompileError> {
    let url = url::Url::parse(value).map_err(|_| CompileError::InvalidUrl(value.to_owned()))?;
    validate_manual_url(&url).map_err(|_| CompileError::InvalidUrl(value.to_owned()))
}

fn normalized_credentials(
    credentials: &[NetworkCredentialIntent],
) -> Result<Vec<&NetworkCredentialIntent>, CompileError> {
    let mut credentials: Vec<_> = credentials.iter().collect();
    credentials.sort_by(|left, right| left.id.cmp(&right.id));
    let mut secrets = HashSet::new();
    for credential in &credentials {
        if credential.id.trim().is_empty()
            || credential.id.trim() != credential.id
            || credential.secret.is_empty()
            || credential.secret.trim() != credential.secret
        {
            return Err(CompileError::InvalidMemberCredential {
                member_id: Uuid::nil(),
                reason: "credential id and secret must be non-empty canonical values".to_owned(),
            });
        }
        if !secrets.insert(credential.secret.as_str()) {
            return Err(CompileError::InvalidMemberCredential {
                member_id: Uuid::nil(),
                reason: format!(
                    "credential {} reuses another credential's key material",
                    credential.id
                ),
            });
        }
        for cidr in &credential.grant.allowed_proxy_cidrs {
            validate_cidr(cidr)?;
        }
        if credential
            .grant
            .acl_groups
            .iter()
            .any(|group| group.trim().is_empty())
        {
            return Err(CompileError::InvalidMemberCredential {
                member_id: Uuid::nil(),
                reason: format!("credential {} contains an empty ACL group", credential.id),
            });
        }
    }
    let runtime_credentials: Vec<_> = credentials
        .iter()
        .map(|credential| compile_managed_credential(credential))
        .collect();
    validate_managed_credential_set(&runtime_credentials).map_err(|reason| {
        CompileError::InvalidMemberCredential {
            member_id: Uuid::nil(),
            reason,
        }
    })?;
    Ok(credentials)
}

fn credentials_by_id<'a>(
    credentials: &'a [&NetworkCredentialIntent],
) -> HashMap<&'a str, &'a NetworkCredentialIntent> {
    credentials
        .iter()
        .map(|credential| (credential.id.as_str(), *credential))
        .collect()
}

fn validate_members(
    intent: &CentralNetworkIntent,
    members: &[&NetworkMemberIntent],
    credentials: &[&NetworkCredentialIntent],
) -> Result<(), CompileError> {
    let mut device_ids = HashSet::new();
    let credential_by_id = credentials_by_id(credentials);

    for member in members {
        if member.device_id.trim().is_empty() || !device_ids.insert(member.device_id.as_str()) {
            return Err(CompileError::DuplicateDevice(member.device_id.clone()));
        }
        let Some(credential_id) = member.credential_id.as_deref() else {
            continue;
        };
        if !intent.secure_mode {
            return Err(CompileError::InvalidMemberCredential {
                member_id: member.id,
                reason: "temporary members require secure mode".to_owned(),
            });
        }
        let credential = credential_by_id.get(credential_id).copied().ok_or(
            CompileError::InvalidMemberCredential {
                member_id: member.id,
                reason: format!("credential {credential_id} does not exist"),
            },
        )?;
        if credential.grant.reusable {
            return Err(CompileError::InvalidMemberCredential {
                member_id: member.id,
                reason: "a member credential must be non-reusable".to_owned(),
            });
        }
        let required_group = member_group_name(member.id);
        if !credential.grant.acl_groups.contains(&required_group) {
            return Err(CompileError::InvalidMemberCredential {
                member_id: member.id,
                reason: format!("grant must contain dedicated ACL group {required_group}"),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Ipv4Subnet {
    network: u32,
    broadcast: u32,
    prefix: u8,
}

fn parse_ipv4_cidr(value: &str) -> Result<Ipv4Subnet, CompileError> {
    let (address, prefix) = value
        .split_once('/')
        .ok_or_else(|| CompileError::InvalidVirtualSubnet(value.to_owned()))?;
    let address: Ipv4Addr = address
        .parse()
        .map_err(|_| CompileError::InvalidVirtualSubnet(value.to_owned()))?;
    let prefix: u8 = prefix
        .parse()
        .ok()
        .filter(|prefix| *prefix <= 32)
        .ok_or_else(|| CompileError::InvalidVirtualSubnet(value.to_owned()))?;
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let network = u32::from(address) & mask;
    Ok(Ipv4Subnet {
        network,
        broadcast: network | !mask,
        prefix,
    })
}

fn assign_addresses(
    virtual_cidr: Option<&str>,
    members: &[&NetworkMemberIntent],
) -> Result<HashMap<Uuid, Option<Ipv4Addr>>, CompileError> {
    let subnet = virtual_cidr.map(parse_ipv4_cidr).transpose()?;
    let mut used = HashSet::new();
    let mut result = HashMap::new();

    for member in members {
        let Some(value) = member.virtual_ipv4.as_deref() else {
            continue;
        };
        let address: Ipv4Addr = value.parse().map_err(|_| CompileError::InvalidMemberIp {
            member_id: member.id,
            address: value.to_owned(),
        })?;
        if let Some(subnet) = subnet {
            let raw = u32::from(address);
            if raw <= subnet.network || raw >= subnet.broadcast {
                return Err(CompileError::InvalidMemberIp {
                    member_id: member.id,
                    address: value.to_owned(),
                });
            }
        }
        if !used.insert(address) {
            return Err(CompileError::DuplicateMemberIp(address));
        }
        result.insert(member.id, Some(address));
    }

    // Explicit addresses take precedence over previous automatic allocations.
    // Reserve every reusable allocation before assigning any new address.
    if let Some(subnet) = subnet {
        for member in members {
            if result.contains_key(&member.id) {
                continue;
            }
            let Some(address) = member
                .allocated_ipv4
                .as_deref()
                .and_then(|value| value.parse::<Ipv4Addr>().ok())
            else {
                continue;
            };
            let raw = u32::from(address);
            if raw > subnet.network && raw < subnet.broadcast && used.insert(address) {
                result.insert(member.id, Some(address));
            }
        }
    }

    for member in members {
        if result.contains_key(&member.id) {
            continue;
        }
        let Some(subnet) = subnet else {
            result.insert(member.id, None);
            continue;
        };
        let address = (subnet.network.saturating_add(1)..subnet.broadcast)
            .map(Ipv4Addr::from)
            .find(|candidate| !used.contains(candidate))
            .ok_or(CompileError::SubnetExhausted(member.id))?;
        used.insert(address);
        result.insert(member.id, Some(address));
    }
    Ok(result)
}

fn validate_cidr(value: &str) -> Result<(), CompileError> {
    value
        .parse::<cidr::IpCidr>()
        .map(|_| ())
        .map_err(|_| CompileError::InvalidCidr(value.to_owned()))
}

fn normalized_grant(grant: &CredentialGrant) -> CredentialGrant {
    let mut grant = grant.clone();
    grant.acl_groups.sort();
    grant.acl_groups.dedup();
    grant.allowed_proxy_cidrs.sort();
    grant.allowed_proxy_cidrs.dedup();
    grant
}

pub(crate) fn compile_managed_credential(
    credential: &NetworkCredentialIntent,
) -> ManagedCredentialConfig {
    let grant = normalized_grant(&credential.grant);
    ManagedCredentialConfig {
        credential_id: credential.id.clone(),
        credential_secret: credential.secret.clone(),
        groups: grant.acl_groups,
        allow_relay: grant.allow_relay,
        allowed_proxy_cidrs: grant.allowed_proxy_cidrs,
        expiry_unix: credential.expiry_unix,
        reusable: Some(grant.reusable),
    }
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
    use easytier::proto::acl::{Acl, AclV1, Chain, ChainType};
    use uuid::Uuid;

    use super::*;
    use crate::central_network::{
        model::{
            AclAction, AclProtocol, AclProtocolTarget, CentralNetworkIntent, CredentialGrant,
            NetworkCredentialIntent, NetworkMemberIntent, NetworkMode, member_group_name,
        },
        network_acl::{AclPeer, AclTarget, NetworkAclGroup, NetworkAclRule},
    };

    fn member(id: u128, device_id: &str, credential_id: Option<&str>) -> NetworkMemberIntent {
        NetworkMemberIntent {
            id: Uuid::from_u128(id),
            device_id: device_id.to_owned(),
            hostname: Some(device_id.to_owned()),
            virtual_ipv4: None,
            allocated_ipv4: None,
            config_override: None,
            credential_id: credential_id.map(str::to_owned),
        }
    }

    fn credential(id: &str, member_id: Option<Uuid>) -> NetworkCredentialIntent {
        NetworkCredentialIntent {
            id: id.to_owned(),
            secret: credential_secret(id),
            expiry_unix: 2_000_000_000,
            grant: CredentialGrant {
                acl_groups: member_id.into_iter().map(member_group_name).collect(),
                allow_relay: true,
                allowed_proxy_cidrs: vec!["10.90.0.0/24".to_owned()],
                reusable: member_id.is_none(),
            },
        }
    }

    fn credential_secret(id: &str) -> String {
        let mut secret = [0u8; 32];
        for (index, byte) in id.bytes().enumerate() {
            secret[index % secret.len()] ^= byte;
        }
        BASE64_STANDARD.encode(secret)
    }

    fn intent() -> CentralNetworkIntent {
        let temporary = member(2, "device-b", Some("temporary"));
        CentralNetworkIntent {
            id: Uuid::from_u128(100),
            user_id: 7,
            display_name: "Engineering".to_owned(),
            network_name: "engineering".to_owned(),
            network_secret: "network-secret".to_owned(),
            mode: NetworkMode::Manual {
                peer_urls: vec!["tcp://gateway.example.com:11010".to_owned()],
            },
            virtual_cidr: Some("10.42.0.0/29".to_owned()),
            secure_mode: true,
            disable_encryption: false,
            encryption_algorithm: None,
            members: vec![temporary.clone(), member(1, "device-a", None)],
            credentials: vec![
                credential("temporary", Some(temporary.id)),
                credential("invite", None),
            ],
        }
    }

    #[test]
    fn compilation_is_deterministic_and_preserves_complete_grants() {
        let original = intent();
        let mut reordered = original.clone();
        reordered.members.reverse();
        reordered.credentials.reverse();

        let first = compile(&original).unwrap();
        let second = compile(&reordered).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.members[0].device_id, "device-a");
        assert_eq!(
            first.members[0].network_config.virtual_ipv4.as_deref(),
            Some("10.42.0.1")
        );

        let managed = &first.members[0].network_config.managed_credentials;
        assert_eq!(managed.len(), 2);
        let temporary = managed
            .iter()
            .find(|item| item.credential_id == "temporary")
            .unwrap();
        assert_eq!(
            temporary.groups,
            vec![member_group_name(Uuid::from_u128(2))]
        );
        assert!(temporary.allow_relay);
        assert_eq!(temporary.allowed_proxy_cidrs, vec!["10.90.0.0/24"]);
        assert_eq!(temporary.reusable, Some(false));
    }

    #[test]
    fn peer_schemes_are_validated_even_without_members() {
        for members in [Vec::new(), intent().members] {
            for url in [
                "tcp://gateway.example.com:11010",
                "http://discovery.example.com/peers",
                "https://discovery.example.com/peers",
                "txt://discovery.example.com",
                "srv://discovery.example.com",
                "bogus://gateway.example.com:11010",
            ] {
                let mut candidate = intent();
                candidate.members = members.clone();
                for mode in [
                    NetworkMode::Manual {
                        peer_urls: vec![url.to_owned()],
                    },
                    NetworkMode::PublicServer {
                        url: url.to_owned(),
                    },
                ] {
                    candidate.mode = mode;
                    let result = compile(&candidate);
                    if url.starts_with("bogus:") {
                        assert_eq!(result, Err(CompileError::InvalidUrl(url.to_owned())));
                    } else {
                        result.unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn member_proxy_networks_must_convert_to_core_config() {
        let mut candidate = intent();
        candidate.members[1].config_override = Some(NetworkConfig {
            proxy_cidrs: vec!["198.18.240.0/24".to_owned()],
            ..Default::default()
        });
        compile(&candidate).unwrap();
        candidate.members[1]
            .config_override
            .as_mut()
            .unwrap()
            .proxy_cidrs
            .push("2001:db8:240::/64".to_owned());
        assert!(matches!(
            compile(&candidate),
            Err(CompileError::InvalidMemberConfig { member_id, reason })
                if member_id == candidate.members[1].id && reason.contains("2001:db8:240::/64")
        ));
    }

    #[test]
    fn member_portal_clients_use_core_validation() {
        use easytier::proto::api::manage::{VpnPortalClientConfig, VpnPortalConfig};

        let mut candidate = intent();
        let client = VpnPortalClientConfig {
            name: "client-one".to_owned(),
            virtual_ip: "10.42.0.5/29".to_owned(),
            groups: vec![member_group_name(Uuid::from_u128(1))],
        };
        let mut portal = VpnPortalConfig {
            enabled: Some(true),
            wireguard_listen: "0.0.0.0:22020".to_owned(),
            wireguard_private_key: Some(credential_secret("portal")),
            clients: vec![client.clone()],
        };
        // Portal clients may only use groups the member's ACL declares.
        let mut overrides = NetworkConfig {
            vpn_portal_config: Some(portal.clone()),
            acl: Some(Acl {
                acl_v1: Some(AclV1 {
                    chains: Vec::new(),
                    group: Some(easytier::proto::acl::GroupInfo {
                        declares: vec![easytier::proto::acl::GroupIdentity {
                            group_name: member_group_name(Uuid::from_u128(1)),
                            group_secret: "group-secret".to_owned(),
                        }],
                        members: Vec::new(),
                    }),
                }),
            }),
            ..Default::default()
        };
        candidate.members[1].config_override = Some(overrides.clone());
        compile(&candidate).unwrap();

        for clients in [
            vec![
                client.clone(),
                VpnPortalClientConfig {
                    name: "client-two".to_owned(),
                    ..client.clone()
                },
            ],
            vec![
                client.clone(),
                VpnPortalClientConfig {
                    virtual_ip: "10.42.0.6/29".to_owned(),
                    ..client.clone()
                },
            ],
            vec![VpnPortalClientConfig {
                virtual_ip: "invalid-ip".to_owned(),
                ..client.clone()
            }],
            vec![VpnPortalClientConfig {
                virtual_ip: "10.42.0.0/29".to_owned(),
                ..client.clone()
            }],
            vec![VpnPortalClientConfig {
                virtual_ip: "10.42.0.7/29".to_owned(),
                ..client.clone()
            }],
            vec![VpnPortalClientConfig {
                virtual_ip: "10.42.0.1/29".to_owned(),
                ..client.clone()
            }],
            vec![VpnPortalClientConfig {
                groups: vec!["not-declared".to_owned()],
                ..client
            }],
        ] {
            portal.clients = clients;
            overrides.vpn_portal_config = Some(portal.clone());
            candidate.members[1].config_override = Some(overrides.clone());
            assert!(matches!(
                compile(&candidate),
                Err(CompileError::InvalidMemberConfig { .. })
            ));
        }
        portal.clients.clear();
        overrides.vpn_portal_config = Some(portal);
        candidate.members[1].config_override = Some(overrides);
        compile(&candidate).unwrap();
    }

    #[test]
    fn temporary_member_only_receives_its_credential_material() {
        let compiled = compile(&intent()).unwrap();
        let temporary = compiled
            .members
            .iter()
            .find(|member| member.device_id == "device-b")
            .unwrap();

        assert_eq!(temporary.network_config.network_secret, None);
        assert!(temporary.network_config.managed_credentials.is_empty());
        assert_eq!(
            temporary
                .network_config
                .secure_mode
                .as_ref()
                .unwrap()
                .local_private_key
                .as_deref(),
            Some(credential_secret("temporary").as_str())
        );

        assert_eq!(temporary.network_config.acl, None);
    }

    fn network_policy() -> NetworkAcl {
        let tcp = AclProtocolTarget {
            protocol: AclProtocol::Tcp,
            ports: vec!["443".to_owned()],
            stateful: true,
        };
        let rule = |id: &str, destination: AclTarget| NetworkAclRule {
            id: id.to_owned(),
            name: id.to_owned(),
            enabled: true,
            action: AclAction::Allow,
            sources: vec![AclPeer::Group {
                name: "admins".to_owned(),
            }],
            destinations: vec![destination],
            protocols: vec![tcp.clone()],
        };
        NetworkAcl {
            default_action: AclAction::Deny,
            groups: vec![NetworkAclGroup {
                name: "admins".to_owned(),
                secret: "admins-secret".to_owned(),
                members: Default::default(),
            }],
            rules: vec![
                rule(
                    "to-temporary",
                    AclTarget::Group {
                        name: member_group_name(Uuid::from_u128(2)),
                    },
                ),
                rule(
                    "to-a-device",
                    AclTarget::Device {
                        instance_id: "other".to_owned(),
                    },
                ),
            ],
            device_secrets: [("other".to_owned(), "device-secret".to_owned())].into(),
        }
    }

    #[test]
    fn temporary_members_run_their_own_acl_then_the_rules_that_reach_them() {
        let mut candidate = intent();
        let own = Acl {
            acl_v1: Some(AclV1 {
                chains: vec![Chain {
                    name: "own".to_owned(),
                    chain_type: ChainType::Inbound as i32,
                    enabled: true,
                    ..Default::default()
                }],
                group: None,
            }),
        };
        candidate.members[0].config_override = Some(NetworkConfig {
            acl: Some(own),
            ..Default::default()
        });
        let compiled = compile_with_acl(&candidate, Some(&network_policy())).unwrap();
        let temporary = compiled
            .members
            .iter()
            .find(|member| member.device_id == "device-b")
            .unwrap();
        let acl = temporary
            .network_config
            .acl
            .as_ref()
            .unwrap()
            .acl_v1
            .as_ref()
            .unwrap();
        let names: Vec<_> = acl.chains.iter().map(|chain| chain.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "own",
                network_acl::INBOUND_CHAIN,
                network_acl::FORWARD_CHAIN
            ]
        );
        assert_eq!(acl.chains[1].rules.len(), 1);
        assert!(acl.chains[1].rules[0].destination_groups.is_empty());
        assert!(
            acl.group.is_none(),
            "credential nodes hold no group secrets"
        );
        let permanent = compiled
            .members
            .iter()
            .find(|member| member.device_id == "device-a")
            .unwrap();
        assert_eq!(permanent.network_config.acl, None);
    }

    #[test]
    fn passwordless_secure_members_preserve_identity_and_grants_after_toml_reload() {
        use easytier::common::config::{ConfigLoader as _, TomlConfigLoader};

        let mut candidate = intent();
        candidate.network_secret.clear();
        let compiled = compile(&candidate).unwrap();
        for member in compiled.members {
            let temporary = member.device_id == "device-b";
            assert_eq!(
                member.network_config.network_secret.as_deref(),
                if temporary { None } else { Some("") }
            );
            let config = member.network_config.gen_config().unwrap();
            let restored = TomlConfigLoader::new_from_str(&config.dump()).unwrap();
            let restored_identity = restored.get_network_identity();
            assert_eq!(
                restored_identity.network_secret,
                config.get_network_identity().network_secret
            );
            assert_eq!(
                restored_identity.network_secret_digest,
                config.get_network_identity().network_secret_digest
            );
            assert_eq!(restored.get_secure_mode(), config.get_secure_mode());
            assert_eq!(
                restored.get_managed_credentials(),
                config.get_managed_credentials()
            );
            let normalized = CoreInstanceConfig::from_toml(&restored).unwrap();
            assert_eq!(
                normalized.managed_credentials.len(),
                if temporary { 0 } else { 2 }
            );
            let output = NetworkConfig::new_from_config(&restored).unwrap();
            assert_eq!(output.network_secret, member.network_config.network_secret);
            assert_eq!(
                output.managed_credentials,
                member.network_config.managed_credentials
            );
            assert_eq!(output.secure_mode, config.get_secure_mode());
        }
    }

    #[test]
    fn validation_rejects_duplicate_devices_and_inconsistent_credentials() {
        let mut duplicate_device = intent();
        duplicate_device.members[1].device_id = duplicate_device.members[0].device_id.clone();
        let error = compile(&duplicate_device).unwrap_err();
        assert!(matches!(error, CompileError::DuplicateDevice(_)));

        let mut reusable = intent();
        reusable.credentials[0].grant.reusable = true;
        let error = compile(&reusable).unwrap_err();
        assert!(matches!(
            error,
            CompileError::InvalidMemberCredential { .. }
        ));

        let mut missing_group = intent();
        missing_group.credentials[0].grant.acl_groups.clear();
        let error = compile(&missing_group).unwrap_err();
        assert!(matches!(
            error,
            CompileError::InvalidMemberCredential { .. }
        ));

        let mut duplicate_secret = intent();
        duplicate_secret.credentials[1].secret = duplicate_secret.credentials[0].secret.clone();
        assert!(matches!(
            compile(&duplicate_secret),
            Err(CompileError::InvalidMemberCredential { .. })
        ));
    }

    #[test]
    fn validation_rejects_bad_network_data() {
        let mut bad_cidr = intent();
        bad_cidr.credentials[0].grant.allowed_proxy_cidrs = vec!["not-a-cidr".to_owned()];
        assert!(matches!(
            compile(&bad_cidr),
            Err(CompileError::InvalidCidr(_))
        ));

        let mut cidr_with_host_bits = intent();
        cidr_with_host_bits.credentials[0].grant.allowed_proxy_cidrs =
            vec!["10.90.0.1/24".to_owned()];
        assert!(matches!(
            compile(&cidr_with_host_bits),
            Err(CompileError::InvalidCidr(_))
        ));

        let mut bad_ip = intent();
        bad_ip.members[0].virtual_ipv4 = Some("192.168.1.1".to_owned());
        assert!(matches!(
            compile(&bad_ip),
            Err(CompileError::InvalidMemberIp { .. })
        ));
    }

    #[test]
    fn validation_uses_runtime_credential_identity_rules() {
        let mut invalid_secret = intent();
        invalid_secret.credentials[0].secret = "not-a-private-key".to_owned();
        assert!(matches!(
            compile(&invalid_secret),
            Err(CompileError::InvalidMemberCredential { .. })
        ));

        let mut noncanonical_id = intent();
        noncanonical_id.credentials[0].id = " temporary".to_owned();
        assert!(matches!(
            compile(&noncanonical_id),
            Err(CompileError::InvalidMemberCredential { .. })
        ));

        let mut noncanonical_secret = intent();
        noncanonical_secret.credentials[0].secret =
            format!(" {}", noncanonical_secret.credentials[0].secret);
        assert!(matches!(
            compile(&noncanonical_secret),
            Err(CompileError::InvalidMemberCredential { .. })
        ));

        let mut duplicate_public_key = intent();
        let original = base64::engine::general_purpose::STANDARD
            .decode(&duplicate_public_key.credentials[0].secret)
            .unwrap();
        let mut equivalent = original;
        equivalent[0] ^= 0b0000_0111;
        duplicate_public_key.credentials[1].secret = BASE64_STANDARD.encode(equivalent);
        assert!(matches!(
            compile(&duplicate_public_key),
            Err(CompileError::InvalidMemberCredential { .. })
        ));
    }

    #[test]
    fn member_override_cannot_replace_central_identity_or_authorization() {
        let mut intent = intent();
        intent.members[1].config_override = Some(NetworkConfig {
            enable_exit_node: Some(true),
            mtu: Some(1300),
            network_name: Some("tampered".to_owned()),
            network_secret: Some("tampered".to_owned()),
            hostname: Some("tampered".to_owned()),
            virtual_ipv4: Some("192.168.1.1".to_owned()),
            dhcp_ipv4_cidr: Some("192.168.100.0/24".to_owned()),
            dhcpv6: Some(true),
            dhcp_ipv6_cidr: Some("fd12::/64".to_owned()),
            managed_credentials: Vec::new(),
            acl: None,
            ..Default::default()
        });

        let compiled = compile(&intent).unwrap();
        let permanent = compiled
            .members
            .iter()
            .find(|member| member.device_id == "device-a")
            .unwrap();
        assert_eq!(permanent.network_config.enable_exit_node, Some(true));
        assert_eq!(permanent.network_config.mtu, Some(1300));
        assert_eq!(
            permanent.network_config.network_name.as_deref(),
            Some("engineering")
        );
        assert_eq!(
            permanent.network_config.network_secret.as_deref(),
            Some("network-secret")
        );
        assert_eq!(
            permanent.network_config.hostname.as_deref(),
            Some("device-a")
        );
        assert_eq!(
            permanent.network_config.virtual_ipv4.as_deref(),
            Some("10.42.0.1")
        );
        assert_eq!(permanent.network_config.managed_credentials.len(), 2);
        assert_eq!(permanent.network_config.dhcp_ipv4_cidr, None);
        assert_eq!(permanent.network_config.dhcpv6, None);
        assert_eq!(permanent.network_config.dhcp_ipv6_cidr, None);
    }
}
