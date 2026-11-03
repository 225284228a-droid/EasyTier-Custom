use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

use easytier::common::config::NetworkConfig;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CentralNetworkIntent {
    pub id: Uuid,
    pub user_id: i32,
    pub display_name: String,
    pub network_name: String,
    pub network_secret: String,
    pub mode: NetworkMode,
    pub virtual_cidr: Option<String>,
    pub secure_mode: bool,
    /// Encryption settings every peer must share; temporary members and the
    /// Gateway follow them.
    #[serde(default)]
    pub disable_encryption: bool,
    #[serde(default)]
    pub encryption_algorithm: Option<String>,
    pub members: Vec<NetworkMemberIntent>,
    pub credentials: Vec<NetworkCredentialIntent>,
}

impl CentralNetworkIntent {
    pub fn remove_device(&mut self, device_id: Uuid) -> bool {
        let removed_members: Vec<_> = self
            .members
            .iter()
            .filter(|member| member.device_id == device_id.to_string())
            .cloned()
            .collect();
        if removed_members.is_empty() {
            return false;
        }
        let removed_member_ids: HashSet<_> =
            removed_members.iter().map(|member| member.id).collect();
        let removed_credential_ids: HashSet<_> = removed_members
            .iter()
            .filter_map(|member| member.credential_id.as_deref())
            .collect();

        self.members
            .retain(|member| !removed_member_ids.contains(&member.id));
        self.credentials
            .retain(|credential| !removed_credential_ids.contains(credential.id.as_str()));
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NetworkMode {
    PublicServer { url: String },
    Manual { peer_urls: Vec<String> },
    Standalone,
    Gateway { peer_url: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetworkMemberIntent {
    pub id: Uuid,
    pub device_id: String,
    pub hostname: Option<String>,
    pub virtual_ipv4: Option<String>,
    #[serde(default)]
    pub allocated_ipv4: Option<String>,
    #[serde(default)]
    pub config_override: Option<NetworkConfig>,
    pub credential_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkCredentialIntent {
    pub id: String,
    pub secret: String,
    pub expiry_unix: i64,
    pub grant: CredentialGrant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialGrant {
    pub acl_groups: Vec<String>,
    pub allow_relay: bool,
    pub allowed_proxy_cidrs: Vec<String>,
    pub reusable: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AclAction {
    #[default]
    Allow,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AclProtocol {
    Tcp,
    Udp,
    Icmp,
    Icmpv6,
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclProtocolTarget {
    pub protocol: AclProtocol,
    pub ports: Vec<String>,
    pub stateful: bool,
}

pub fn member_group_name(member_id: Uuid) -> String {
    format!("member:{member_id}")
}
