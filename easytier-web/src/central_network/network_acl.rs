//! A network's access control, held identically by every permanent member.
//!
//! Rules name groups, never members, so each member receives the same two
//! chains (`network:inbound` and `network:forward`) and a member joining or
//! leaving changes nobody else. Group secrets are generated once and stored
//! in the members' group declarations; which groups a member belongs to is
//! that member's own `group.members`. A console therefore rebuilds the whole
//! policy from what the devices hold: the rules and secrets from any member,
//! the memberships from each member.
//!
//! The network chains follow the device's own chains, so a device's own rules
//! are matched first, and its own chain's default action wins over the
//! network's when it has one.
//!
//! A single device is the implicit group `device:<instance id>`, declared
//! only while a rule names it. A temporary device is the group of its
//! credential, `member:<member id>`, and joins custom groups through its
//! credential grant; credential nodes cannot verify group proofs, so their
//! copy of the rules is cut down to the rules that reach them.

use std::collections::{BTreeMap, BTreeSet};

use easytier::proto::{
    acl::{Acl, Action, Chain, ChainType, GroupIdentity, GroupInfo, Protocol, Rule},
    api::manage::NetworkConfig,
};
use serde::{Deserialize, Serialize};

use super::model::{AclAction, AclProtocol, AclProtocolTarget};

pub const INBOUND_CHAIN: &str = "network:inbound";
pub const FORWARD_CHAIN: &str = "network:forward";
/// Marks a network chain; the inbound one also lists the network's groups.
const MARK: &str = "easytier-network-acl";
const DEVICE_GROUP: &str = "device:";
/// A temporary device's credential group, see `model::member_group_name`.
const CREDENTIAL_GROUP: &str = "member:";
const MAX_GROUPS: usize = 256;
const MAX_RULES: usize = 512;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAcl {
    #[serde(default)]
    pub default_action: AclAction,
    #[serde(default)]
    pub groups: Vec<NetworkAclGroup>,
    #[serde(default)]
    pub rules: Vec<NetworkAclRule>,
    /// Secrets of the device groups rules name, by instance id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub device_secrets: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAclGroup {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret: String,
    /// Permanent members by instance id, temporary ones by credential group.
    #[serde(default)]
    pub members: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAclRule {
    pub id: String,
    pub name: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub action: AclAction,
    pub sources: Vec<AclPeer>,
    pub destinations: Vec<AclTarget>,
    pub protocols: Vec<AclProtocolTarget>,
}

fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AclPeer {
    All,
    Group { name: String },
    Device { instance_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AclTarget {
    All,
    Group { name: String },
    Device { instance_id: String },
    Subnet { cidrs: Vec<String> },
}

/// The network part of one member's ACL.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberAcl {
    /// The rules and secrets it holds, memberships left out.
    #[serde(default)]
    pub network: Option<NetworkAcl>,
    /// The network groups it belongs to.
    #[serde(default)]
    pub groups: BTreeSet<String>,
    /// Whether its own inbound chain comes first and so sets the default.
    #[serde(default)]
    pub own_default: bool,
}

fn peer_group(peer: &AclPeer) -> Option<String> {
    match peer {
        AclPeer::All => None,
        AclPeer::Group { name } => Some(name.clone()),
        AclPeer::Device { instance_id } => Some(format!("{DEVICE_GROUP}{instance_id}")),
    }
}

fn peer_of_group(name: &str) -> AclPeer {
    match name.strip_prefix(DEVICE_GROUP) {
        Some(instance_id) => AclPeer::Device {
            instance_id: instance_id.to_owned(),
        },
        None => AclPeer::Group {
            name: name.to_owned(),
        },
    }
}

fn target_of_peer(peer: AclPeer) -> AclTarget {
    match peer {
        AclPeer::All => AclTarget::All,
        AclPeer::Group { name } => AclTarget::Group { name },
        AclPeer::Device { instance_id } => AclTarget::Device { instance_id },
    }
}

fn valid_group_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn validate_protocol(rule: &str, target: &AclProtocolTarget) -> anyhow::Result<()> {
    anyhow::ensure!(
        !target.stateful || target.protocol == AclProtocol::Tcp,
        "rule {rule}: stateful is only supported for TCP"
    );
    anyhow::ensure!(
        !matches!(target.protocol, AclProtocol::Icmp | AclProtocol::Icmpv6)
            || target.ports.is_empty(),
        "rule {rule}: ICMP protocols do not accept ports"
    );
    for port in &target.ports {
        let valid = match port.split_once('-') {
            Some((start, end)) => matches!(
                (start.parse::<u16>(), end.parse::<u16>()),
                (Ok(start), Ok(end)) if start > 0 && start <= end
            ),
            None => port.parse::<u16>().is_ok_and(|port| port > 0),
        };
        anyhow::ensure!(valid, "rule {rule}: invalid port {port}");
    }
    Ok(())
}

impl NetworkAcl {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.groups.len() <= MAX_GROUPS, "too many groups");
        anyhow::ensure!(self.rules.len() <= MAX_RULES, "too many rules");
        let mut names = BTreeSet::new();
        for group in &self.groups {
            anyhow::ensure!(
                valid_group_name(&group.name),
                "invalid group name {:?}: use letters, digits, '_', '-' or '.'",
                group.name
            );
            anyhow::ensure!(
                names.insert(group.name.as_str()),
                "duplicate group {}",
                group.name
            );
        }
        let known = |name: &str| names.contains(name) || name.starts_with(CREDENTIAL_GROUP);
        let mut ids = BTreeSet::new();
        for rule in &self.rules {
            anyhow::ensure!(
                !rule.id.trim().is_empty() && ids.insert(rule.id.as_str()),
                "duplicate or empty rule id {:?}",
                rule.id
            );
            anyhow::ensure!(
                !rule.name.trim().is_empty()
                    && !rule.sources.is_empty()
                    && !rule.destinations.is_empty()
                    && !rule.protocols.is_empty(),
                "rule {}: name, sources, destinations and protocols must not be empty",
                rule.id
            );
            let peers = rule
                .sources
                .iter()
                .cloned()
                .chain(rule.destinations.iter().filter_map(|target| match target {
                    AclTarget::All => Some(AclPeer::All),
                    AclTarget::Group { name } => Some(AclPeer::Group { name: name.clone() }),
                    AclTarget::Device { instance_id } => Some(AclPeer::Device {
                        instance_id: instance_id.clone(),
                    }),
                    AclTarget::Subnet { .. } => None,
                }));
            for peer in peers {
                match &peer {
                    AclPeer::All => {}
                    AclPeer::Group { name } => {
                        anyhow::ensure!(known(name), "rule {}: unknown group {name}", rule.id)
                    }
                    AclPeer::Device { instance_id } => anyhow::ensure!(
                        !instance_id.is_empty() && !instance_id.contains(','),
                        "rule {}: invalid device {instance_id:?}",
                        rule.id
                    ),
                }
            }
            for target in &rule.destinations {
                if let AclTarget::Subnet { cidrs } = target {
                    anyhow::ensure!(
                        !cidrs.is_empty(),
                        "rule {}: a subnet needs at least one CIDR",
                        rule.id
                    );
                    for cidr in cidrs {
                        cidr.parse::<cidr::IpCidr>().map_err(|_| {
                            anyhow::anyhow!("rule {}: invalid CIDR {cidr}", rule.id)
                        })?;
                    }
                }
            }
            for target in &rule.protocols {
                validate_protocol(&rule.id, target)?;
            }
        }
        Ok(())
    }

    /// The same policy in one canonical form, so copies compare equal.
    pub fn normalized(mut self) -> Self {
        self.groups
            .sort_by(|left, right| left.name.cmp(&right.name));
        for rule in &mut self.rules {
            rule.sources = normalized_peers(std::mem::take(&mut rule.sources));
            rule.destinations = normalized_targets(std::mem::take(&mut rule.destinations));
            let mut protocols = Vec::new();
            for mut protocol in std::mem::take(&mut rule.protocols) {
                protocol.ports.sort();
                protocol.ports.dedup();
                if !protocols.contains(&protocol) {
                    protocols.push(protocol);
                }
            }
            rule.protocols = protocols;
        }
        self
    }

    fn device_groups(&self) -> BTreeSet<String> {
        let mut devices = BTreeSet::new();
        for rule in &self.rules {
            for source in &rule.sources {
                if let AclPeer::Device { instance_id } = source {
                    devices.insert(instance_id.clone());
                }
            }
            for target in &rule.destinations {
                if let AclTarget::Device { instance_id } = target {
                    devices.insert(instance_id.clone());
                }
            }
        }
        devices
    }

    /// Give every group and named device a secret: the one `current` holds
    /// under the same name, or a new one. Devices no rule names lose theirs.
    pub fn with_secrets(
        mut self,
        current: Option<&NetworkAcl>,
        new_secret: impl Fn() -> String,
    ) -> Self {
        for group in &mut self.groups {
            group.secret = current
                .and_then(|current| current.groups.iter().find(|known| known.name == group.name))
                .map(|known| known.secret.clone())
                .filter(|secret| !secret.is_empty())
                .unwrap_or_else(&new_secret);
        }
        self.device_secrets = self
            .device_groups()
            .into_iter()
            .map(|device| {
                let secret = current
                    .and_then(|current| current.device_secrets.get(&device))
                    .filter(|secret| !secret.is_empty())
                    .cloned()
                    .unwrap_or_else(&new_secret);
                (device, secret)
            })
            .collect();
        self
    }

    /// The policy without its secrets, for the browser.
    pub fn without_secrets(mut self) -> Self {
        for group in &mut self.groups {
            group.secret.clear();
        }
        self.device_secrets.clear();
        self
    }

    /// The copy a temporary device runs: it cannot tell which groups other
    /// nodes belong to, so rules aimed at a group keep only the groups it is
    /// in, and match it as their only destination.
    pub fn for_credential_member(&self, groups: &BTreeSet<String>) -> NetworkAcl {
        let mut acl = self.clone().without_secrets();
        acl.groups.clear();
        acl.rules.retain_mut(|rule| {
            let reaches = rule.destinations.iter().any(|target| match target {
                AclTarget::All => true,
                AclTarget::Group { name } => groups.contains(name),
                AclTarget::Device { .. } | AclTarget::Subnet { .. } => false,
            });
            let subnets = rule
                .destinations
                .iter()
                .filter(|target| matches!(target, AclTarget::Subnet { .. }))
                .cloned();
            rule.destinations = reaches
                .then_some(AclTarget::All)
                .into_iter()
                .chain(subnets)
                .collect();
            !rule.destinations.is_empty()
        });
        acl
    }

    /// The network's chains, which follow a device's own.
    pub fn chains(&self) -> Vec<Chain> {
        let action = core_action(self.default_action) as i32;
        let mut inbound = Vec::new();
        let mut forward = Vec::new();
        let count = self.rules.len();
        for (position, rule) in self.rules.iter().enumerate() {
            let priority = ((count - position) * 10) as u32;
            let mut sources: Vec<_> = if rule.sources.contains(&AclPeer::All) {
                Vec::new()
            } else {
                rule.sources.iter().filter_map(peer_group).collect()
            };
            sources.sort();
            sources.dedup();
            let peers: Vec<_> = rule
                .destinations
                .iter()
                .filter(|target| !matches!(target, AclTarget::Subnet { .. }))
                .collect();
            let destinations: Option<Vec<String>> = (!peers.is_empty()).then(|| {
                if peers.contains(&&AclTarget::All) {
                    return Vec::new();
                }
                let mut groups: Vec<_> = peers
                    .iter()
                    .filter_map(|target| match target {
                        AclTarget::Group { name } => Some(name.clone()),
                        AclTarget::Device { instance_id } => {
                            Some(format!("{DEVICE_GROUP}{instance_id}"))
                        }
                        _ => None,
                    })
                    .collect();
                groups.sort();
                groups.dedup();
                groups
            });
            let mut cidrs: Vec<String> = rule
                .destinations
                .iter()
                .filter_map(|target| match target {
                    AclTarget::Subnet { cidrs } => Some(cidrs.clone()),
                    _ => None,
                })
                .flatten()
                .collect();
            cidrs.sort();
            cidrs.dedup();
            for protocol in &rule.protocols {
                let core_rule =
                    |destination_ips: Vec<String>, destination_groups: Vec<String>| Rule {
                        name: format!("{}:{}", rule.id, protocol_name(protocol.protocol)),
                        description: rule.name.clone(),
                        priority,
                        enabled: rule.enabled,
                        protocol: core_protocol(protocol.protocol) as i32,
                        ports: protocol.ports.clone(),
                        source_ips: Vec::new(),
                        destination_ips,
                        source_ports: Vec::new(),
                        action: core_action(rule.action) as i32,
                        rate_limit: 0,
                        burst_limit: 0,
                        stateful: protocol.stateful,
                        source_groups: sources.clone(),
                        destination_groups,
                    };
                if let Some(groups) = &destinations {
                    inbound.push(core_rule(Vec::new(), groups.clone()));
                }
                if !cidrs.is_empty() {
                    forward.push(core_rule(cidrs.clone(), Vec::new()));
                }
            }
        }
        let mut names: Vec<_> = self
            .groups
            .iter()
            .map(|group| group.name.clone())
            .chain(
                self.device_secrets
                    .keys()
                    .map(|device| format!("{DEVICE_GROUP}{device}")),
            )
            .collect();
        names.sort();
        vec![
            Chain {
                name: INBOUND_CHAIN.to_owned(),
                chain_type: ChainType::Inbound as i32,
                description: format!("{MARK} groups={}", names.join(",")),
                enabled: true,
                rules: inbound,
                default_action: action,
            },
            Chain {
                name: FORWARD_CHAIN.to_owned(),
                chain_type: ChainType::Forward as i32,
                description: MARK.to_owned(),
                enabled: true,
                rules: forward,
                default_action: action,
            },
        ]
    }
}

fn normalized_peers(peers: Vec<AclPeer>) -> Vec<AclPeer> {
    if peers.contains(&AclPeer::All) {
        return vec![AclPeer::All];
    }
    let set: BTreeSet<_> = peers.into_iter().collect();
    set.into_iter().collect()
}

fn normalized_targets(targets: Vec<AclTarget>) -> Vec<AclTarget> {
    let mut cidrs = BTreeSet::new();
    let mut peers = Vec::new();
    for target in targets {
        match target {
            AclTarget::Subnet { cidrs: more } => cidrs.extend(more),
            AclTarget::All => peers.push(AclPeer::All),
            AclTarget::Group { name } => peers.push(AclPeer::Group { name }),
            AclTarget::Device { instance_id } => peers.push(AclPeer::Device { instance_id }),
        }
    }
    let mut targets: Vec<_> = if peers.is_empty() {
        Vec::new()
    } else {
        normalized_peers(peers)
            .into_iter()
            .map(target_of_peer)
            .collect()
    };
    if !cidrs.is_empty() {
        targets.push(AclTarget::Subnet {
            cidrs: cidrs.into_iter().collect(),
        });
    }
    targets
}

fn is_network_chain(chain: &Chain) -> bool {
    chain.name == INBOUND_CHAIN || chain.name == FORWARD_CHAIN
}

/// The group names the network declared, read from its inbound chain.
fn declared_names(chain: &Chain) -> BTreeSet<String> {
    chain
        .description
        .split_once("groups=")
        .map(|(_, names)| {
            names
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The network part of `acl`, as one member holds it.
pub fn read(acl: Option<&Acl>) -> MemberAcl {
    let Some(acl) = acl.and_then(|acl| acl.acl_v1.as_ref()) else {
        return MemberAcl::default();
    };
    let own_default = acl
        .chains
        .iter()
        .find(|chain| chain.chain_type == ChainType::Inbound as i32)
        .is_some_and(|chain| !is_network_chain(chain));
    let Some(inbound) = acl
        .chains
        .iter()
        .find(|chain| chain.name == INBOUND_CHAIN && chain.chain_type == ChainType::Inbound as i32)
    else {
        return MemberAcl {
            own_default,
            ..Default::default()
        };
    };
    let forward = acl
        .chains
        .iter()
        .find(|chain| chain.name == FORWARD_CHAIN && chain.chain_type == ChainType::Forward as i32);
    let names = declared_names(inbound);
    let empty = GroupInfo::default();
    let group = acl.group.as_ref().unwrap_or(&empty);
    let secret = |name: &str| {
        group
            .declares
            .iter()
            .find(|declared| declared.group_name == name)
            .map(|declared| declared.group_secret.clone())
            .unwrap_or_default()
    };
    let mut network = NetworkAcl {
        default_action: acl_action(inbound.default_action()),
        ..Default::default()
    };
    for name in &names {
        match name.strip_prefix(DEVICE_GROUP) {
            Some(device) => {
                network
                    .device_secrets
                    .insert(device.to_owned(), secret(name));
            }
            None => network.groups.push(NetworkAclGroup {
                name: name.clone(),
                secret: secret(name),
                members: BTreeSet::new(),
            }),
        }
    }

    // Rules in priority order; one network rule became a core rule per
    // protocol and chain.
    let mut core_rules: Vec<(bool, &Rule)> = inbound
        .rules
        .iter()
        .map(|rule| (false, rule))
        .chain(
            forward
                .into_iter()
                .flat_map(|chain| chain.rules.iter().map(|rule| (true, rule))),
        )
        .collect();
    core_rules.sort_by_key(|(_, rule)| std::cmp::Reverse(rule.priority));
    let mut rules: Vec<NetworkAclRule> = Vec::new();
    for (forwarded, core) in core_rules {
        let id = core
            .name
            .rsplit_once(':')
            .map_or(core.name.as_str(), |(id, _)| id);
        let position = match rules.iter().position(|rule| rule.id == id) {
            Some(position) => position,
            None => {
                rules.push(NetworkAclRule {
                    id: id.to_owned(),
                    name: core.description.clone(),
                    enabled: core.enabled,
                    action: acl_action(core.action()),
                    sources: if core.source_groups.is_empty() {
                        vec![AclPeer::All]
                    } else {
                        core.source_groups
                            .iter()
                            .map(|name| peer_of_group(name))
                            .collect()
                    },
                    destinations: Vec::new(),
                    protocols: Vec::new(),
                });
                rules.len() - 1
            }
        };
        let rule = &mut rules[position];
        if forwarded {
            rule.destinations.push(AclTarget::Subnet {
                cidrs: core.destination_ips.clone(),
            });
        } else if core.destination_groups.is_empty() {
            rule.destinations.push(AclTarget::All);
        } else {
            rule.destinations.extend(
                core.destination_groups
                    .iter()
                    .map(|name| target_of_peer(peer_of_group(name))),
            );
        }
        rule.protocols.push(AclProtocolTarget {
            protocol: acl_protocol(core.protocol()),
            ports: core.ports.clone(),
            stateful: core.stateful,
        });
    }
    network.rules = rules;
    MemberAcl {
        network: Some(network.normalized()),
        groups: group
            .members
            .iter()
            .filter(|name| names.contains(*name))
            .cloned()
            .collect(),
        own_default,
    }
}

/// Give `config` the network's chains and groups, or remove them for `None`.
/// The device's own chains and groups stay; the network's follow them.
pub fn apply(config: &mut NetworkConfig, acl: Option<&NetworkAcl>) {
    let holds_network = config
        .acl
        .as_ref()
        .and_then(|acl| acl.acl_v1.as_ref())
        .is_some_and(|acl| acl.chains.iter().any(is_network_chain));
    if acl.is_none() && !holds_network {
        return;
    }
    let instance_id = config.instance_id.clone().unwrap_or_default();
    let acl_v1 = config
        .acl
        .get_or_insert_with(Acl::default)
        .acl_v1
        .get_or_insert_with(Default::default);
    let previous: BTreeSet<String> = acl_v1
        .chains
        .iter()
        .filter(|chain| chain.name == INBOUND_CHAIN)
        .flat_map(declared_names)
        .collect();
    acl_v1.chains.retain(|chain| !is_network_chain(chain));
    let mut group = acl_v1.group.take().unwrap_or_default();
    group
        .declares
        .retain(|declared| !previous.contains(&declared.group_name));
    group.members.retain(|name| !previous.contains(name));
    if let Some(acl) = acl {
        acl_v1.chains.extend(acl.chains());
        let own: BTreeSet<_> = group
            .declares
            .iter()
            .map(|declared| declared.group_name.clone())
            .collect();
        let declares = acl
            .groups
            .iter()
            .map(|group| (group.name.clone(), group.secret.clone()))
            .chain(
                acl.device_secrets
                    .iter()
                    .map(|(device, secret)| (format!("{DEVICE_GROUP}{device}"), secret.clone())),
            );
        for (group_name, group_secret) in declares {
            // The network's group replaces a device group of the same name.
            if own.contains(&group_name) {
                group
                    .declares
                    .retain(|declared| declared.group_name != group_name);
            }
            group.declares.push(GroupIdentity {
                group_name,
                group_secret,
            });
        }
        let joined = acl
            .groups
            .iter()
            .filter(|group| group.members.contains(&instance_id))
            .map(|group| group.name.clone())
            .chain(
                acl.device_secrets
                    .contains_key(&instance_id)
                    .then(|| format!("{DEVICE_GROUP}{instance_id}")),
            );
        for name in joined {
            if !group.members.contains(&name) {
                group.members.push(name);
            }
        }
    }
    if !group.declares.is_empty() || !group.members.is_empty() {
        acl_v1.group = Some(group);
    }
    if acl_v1.chains.is_empty() && acl_v1.group.is_none() {
        config.acl = None;
    }
}

/// The network's policy as its permanent members hold it: the copy on the
/// member another console wrote last, else the most common one, with every
/// member's own memberships.
pub fn current<'a>(
    members: impl IntoIterator<Item = (&'a str, Option<i64>, &'a MemberAcl)>,
) -> Option<NetworkAcl> {
    let members: Vec<_> = members.into_iter().collect();
    let newest = members.iter().map(|(_, stamp, _)| *stamp).max()?;
    let mut counts: BTreeMap<String, (usize, &Option<NetworkAcl>)> = BTreeMap::new();
    for (_, stamp, member) in &members {
        if *stamp == newest {
            let key = serde_json::to_string(&member.network).unwrap_or_default();
            counts.entry(key).or_insert((0, &member.network)).0 += 1;
        }
    }
    let mut chosen = counts
        .into_values()
        .max_by_key(|(count, _)| *count)
        .and_then(|(_, network)| network.clone())?;
    let names: BTreeSet<_> = chosen
        .groups
        .iter()
        .map(|group| group.name.clone())
        .collect();
    for (instance_id, _, member) in &members {
        for name in member.groups.intersection(&names) {
            if let Some(group) = chosen.groups.iter_mut().find(|group| &group.name == name) {
                group.members.insert((*instance_id).to_owned());
            }
        }
    }
    Some(chosen)
}

/// The policy without memberships, for comparing members' copies.
pub fn rules_of(acl: &NetworkAcl) -> NetworkAcl {
    let mut acl = acl.clone();
    for group in &mut acl.groups {
        group.members.clear();
    }
    acl
}

fn core_action(action: AclAction) -> Action {
    match action {
        AclAction::Allow => Action::Allow,
        AclAction::Deny => Action::Drop,
    }
}

fn acl_action(action: Action) -> AclAction {
    match action {
        Action::Drop => AclAction::Deny,
        Action::Allow | Action::Noop => AclAction::Allow,
    }
}

fn core_protocol(protocol: AclProtocol) -> Protocol {
    match protocol {
        AclProtocol::Tcp => Protocol::Tcp,
        AclProtocol::Udp => Protocol::Udp,
        AclProtocol::Icmp => Protocol::Icmp,
        AclProtocol::Icmpv6 => Protocol::IcmPv6,
        AclProtocol::Any => Protocol::Any,
    }
}

fn acl_protocol(protocol: Protocol) -> AclProtocol {
    match protocol {
        Protocol::Tcp => AclProtocol::Tcp,
        Protocol::Udp => AclProtocol::Udp,
        Protocol::Icmp => AclProtocol::Icmp,
        Protocol::IcmPv6 => AclProtocol::Icmpv6,
        Protocol::Any | Protocol::Unspecified => AclProtocol::Any,
    }
}

fn protocol_name(protocol: AclProtocol) -> &'static str {
    match protocol {
        AclProtocol::Tcp => "tcp",
        AclProtocol::Udp => "udp",
        AclProtocol::Icmp => "icmp",
        AclProtocol::Icmpv6 => "icmpv6",
        AclProtocol::Any => "any",
    }
}

#[cfg(test)]
mod tests {
    use easytier::proto::acl::AclV1;

    use super::*;

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const PEER: &str = "22222222-2222-2222-2222-222222222222";

    fn tcp(ports: &[&str]) -> AclProtocolTarget {
        AclProtocolTarget {
            protocol: AclProtocol::Tcp,
            ports: ports.iter().map(|port| (*port).to_owned()).collect(),
            stateful: true,
        }
    }

    fn policy() -> NetworkAcl {
        NetworkAcl {
            default_action: AclAction::Deny,
            groups: vec![
                NetworkAclGroup {
                    name: "admins".into(),
                    secret: String::new(),
                    members: [ME.to_owned()].into(),
                },
                NetworkAclGroup {
                    name: "servers".into(),
                    secret: String::new(),
                    members: [PEER.to_owned(), "member:temp".to_owned()].into(),
                },
            ],
            rules: vec![
                NetworkAclRule {
                    id: "ssh".into(),
                    name: "Admins reach servers".into(),
                    enabled: true,
                    action: AclAction::Allow,
                    sources: vec![AclPeer::Group {
                        name: "admins".into(),
                    }],
                    destinations: vec![
                        AclTarget::Group {
                            name: "servers".into(),
                        },
                        AclTarget::Subnet {
                            cidrs: vec!["192.168.1.0/24".into()],
                        },
                    ],
                    protocols: vec![
                        tcp(&["22"]),
                        AclProtocolTarget {
                            protocol: AclProtocol::Icmp,
                            ports: Vec::new(),
                            stateful: false,
                        },
                    ],
                },
                NetworkAclRule {
                    id: "peer".into(),
                    name: "Peer reaches me".into(),
                    enabled: false,
                    action: AclAction::Deny,
                    sources: vec![AclPeer::Device {
                        instance_id: PEER.into(),
                    }],
                    destinations: vec![AclTarget::Device {
                        instance_id: ME.into(),
                    }],
                    protocols: vec![AclProtocolTarget {
                        protocol: AclProtocol::Any,
                        ports: Vec::new(),
                        stateful: false,
                    }],
                },
            ],
            device_secrets: BTreeMap::new(),
        }
        .normalized()
        .with_secrets(None, || "secret".to_owned())
    }

    fn config(instance_id: &str) -> NetworkConfig {
        NetworkConfig {
            instance_id: Some(instance_id.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn a_member_copy_reads_back_as_the_same_policy() {
        let acl = policy();
        acl.validate().unwrap();
        let mut me = config(ME);
        apply(&mut me, Some(&acl));
        let read = read(me.acl.as_ref());
        assert_eq!(read.network.as_ref(), Some(&rules_of(&acl)));
        assert_eq!(
            read.groups,
            ["admins".to_owned(), format!("device:{ME}")].into()
        );
        assert!(!read.own_default);
    }

    #[test]
    fn every_member_holds_the_same_rules_and_its_own_groups() {
        let acl = policy();
        let mut me = config(ME);
        let mut peer = config(PEER);
        apply(&mut me, Some(&acl));
        apply(&mut peer, Some(&acl));
        let chains = |config: &NetworkConfig| config.acl.clone().unwrap().acl_v1.unwrap().chains;
        assert_eq!(chains(&me), chains(&peer));
        let me_read = read(me.acl.as_ref());
        let peer_read = read(peer.acl.as_ref());
        let rebuilt = current([(ME, Some(5), &me_read), (PEER, Some(5), &peer_read)]).unwrap();
        let mut expected = acl.clone();
        expected.groups[1].members.remove("member:temp");
        assert_eq!(rebuilt, expected);
    }

    #[test]
    fn device_chains_stay_first_and_removal_restores_the_device_acl() {
        let own = Acl {
            acl_v1: Some(AclV1 {
                chains: vec![Chain {
                    name: "mine".into(),
                    chain_type: ChainType::Inbound as i32,
                    enabled: true,
                    default_action: Action::Allow as i32,
                    ..Default::default()
                }],
                group: Some(GroupInfo {
                    declares: vec![GroupIdentity {
                        group_name: "home".into(),
                        group_secret: "mine".into(),
                    }],
                    members: vec!["home".into()],
                }),
            }),
        };
        let mut me = config(ME);
        me.acl = Some(own.clone());
        apply(&mut me, Some(&policy()));
        let chains = &me.acl.as_ref().unwrap().acl_v1.as_ref().unwrap().chains;
        assert_eq!(chains[0].name, "mine");
        assert_eq!(chains[1].name, INBOUND_CHAIN);
        assert!(read(me.acl.as_ref()).own_default);
        apply(&mut me, None);
        assert_eq!(me.acl, Some(own));
    }

    #[test]
    fn removing_the_policy_from_a_device_without_its_own_acl_clears_it() {
        let mut me = config(ME);
        apply(&mut me, Some(&policy()));
        apply(&mut me, None);
        assert_eq!(me.acl, None);
        apply(&mut me, None);
        assert_eq!(me.acl, None);
    }

    #[test]
    fn secrets_survive_edits_and_unnamed_devices_lose_theirs() {
        let acl = policy();
        assert!(acl.device_secrets.contains_key(ME));
        let mut edited = acl.clone().without_secrets();
        edited.rules.truncate(1);
        let edited = edited.with_secrets(Some(&acl), || "new".to_owned());
        assert!(edited.groups.iter().all(|group| group.secret == "secret"));
        assert!(edited.device_secrets.is_empty());
    }

    #[test]
    fn the_newest_copy_wins_over_the_most_common_one() {
        let acl = policy();
        let mut old = acl.clone();
        old.default_action = AclAction::Allow;
        let member = |network: &NetworkAcl| MemberAcl {
            network: Some(rules_of(network)),
            ..Default::default()
        };
        let (newest, stale) = (member(&acl), member(&old));
        let chosen = current([
            ("a", Some(1), &stale),
            ("b", Some(1), &stale),
            ("c", Some(2), &newest),
        ])
        .unwrap();
        assert_eq!(chosen.default_action, AclAction::Deny);
        assert!(
            current([
                ("a", Some(3), &MemberAcl::default()),
                ("c", Some(2), &newest)
            ])
            .is_none()
        );
    }

    #[test]
    fn temporary_devices_keep_rules_that_reach_them_aimed_at_themselves() {
        let acl = policy();
        let temp = acl.for_credential_member(&["servers".to_owned()].into());
        assert_eq!(temp.rules.len(), 1);
        assert_eq!(
            temp.rules[0].destinations,
            vec![
                AclTarget::All,
                AclTarget::Subnet {
                    cidrs: vec!["192.168.1.0/24".into()]
                }
            ]
        );
        assert!(temp.groups.is_empty() && temp.device_secrets.is_empty());
        let other = acl.for_credential_member(&BTreeSet::new());
        assert_eq!(other.rules.len(), 1, "the subnet part still applies");
    }

    #[test]
    fn validation_rejects_unknown_groups_and_bad_targets() {
        let mut acl = policy();
        acl.rules[0].sources = vec![AclPeer::Group {
            name: "nobody".into(),
        }];
        assert!(acl.validate().is_err());
        let mut acl = policy();
        acl.rules[0].sources = vec![AclPeer::Group {
            name: "member:temp".into(),
        }];
        assert!(
            acl.validate().is_ok(),
            "credential groups need no declaration"
        );
        let mut acl = policy();
        acl.groups[0].name = "device:x".into();
        assert!(acl.validate().is_err());
        let mut acl = policy();
        acl.rules[0].protocols = vec![AclProtocolTarget {
            protocol: AclProtocol::Udp,
            ports: Vec::new(),
            stateful: true,
        }];
        assert!(acl.validate().is_err());
    }
}
