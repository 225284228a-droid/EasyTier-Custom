//! Console-facing central-network API.
//!
//! These endpoints are custom additions to the upstream service; they live
//! here so `service.rs` stays close to its original shape.

use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter as _};
use uuid::Uuid;

use crate::{
    central_network::{
        advanced::{self, AdvancedChange, AdvancedValues},
        core_sync::{self, CoreChange, CoreSettings, Ipv4Inet, MemberSyncResult},
        mesh_network::{self, JoinResult, MeshNetworkView, Preset},
        model::{CentralNetworkIntent, member_group_name},
        network_acl::{self, MemberAcl, NetworkAcl},
    },
    db::UserIdInDb,
};

use super::{
    CentralNetworkService, CentralNetworkServiceError, intent_core, invalid,
    normalize_network_secret, random_secret,
};

/// A network's access control as the console rebuilt it from its members.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NetworkAclView {
    /// The policy without its secrets; none when the network has none.
    /// Temporary devices appear in groups by their credential group.
    pub acl: Option<NetworkAcl>,
    pub members: Vec<MemberAclStatus>,
    pub temporary: Vec<TemporaryAclMember>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemberAclStatus {
    pub device_id: String,
    pub instance_id: String,
    /// Holds the network's rules as they are now.
    pub in_sync: bool,
    /// Its own inbound chain comes first, so its default action applies
    /// instead of the network's.
    pub own_default: bool,
}

/// A temporary device, named in rules by its credential group.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TemporaryAclMember {
    pub member_id: Uuid,
    pub device_id: String,
    pub hostname: Option<String>,
    pub group: String,
}

/// A network instance seen on a device, without its secret.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MeshMemberInfo {
    pub device_id: String,
    pub instance_id: String,
    pub network_name: String,
    /// The name its configuration shows, when it has its own.
    pub display_name: Option<String>,
    pub online: bool,
    /// A missed core change waits for this member to connect.
    pub pending: bool,
    pub sync_at: Option<i64>,
    /// A temporary member, authenticated by a credential.
    pub credential: bool,
    /// This console's batch edits leave its advanced settings and running
    /// state alone.
    pub locked: bool,
    /// The advanced settings it held when last seen.
    pub advanced: AdvancedValues,
    /// Its static address with the prefix; DHCP members have none.
    pub virtual_ipv4: Option<String>,
}

/// A new network: only the name is required.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CreateMeshNetworkReq {
    pub network_name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Unset generates one; an empty string is a passwordless network.
    #[serde(default)]
    pub network_secret: Option<String>,
    #[serde(default)]
    pub secure_mode: bool,
    #[serde(default)]
    pub preset: Option<Preset>,
    /// The network's IPv4 subnet; unset leaves DHCP at its default.
    #[serde(default)]
    pub virtual_subnet: Option<String>,
    /// Devices that join right away.
    #[serde(default)]
    pub devices: Vec<Uuid>,
}

impl CentralNetworkService {
    /// Change the core settings of the network named `network_name` on every
    /// member this console knows. A central network of that name follows,
    /// so its temporary members and Gateway keep matching.
    pub async fn apply_core_change(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
        change: CoreChange,
    ) -> Result<Vec<MemberSyncResult>, CentralNetworkServiceError> {
        let _mutation = self.lock_mutations(user_id).await;
        if let Some(mut intent) = self.load_by_name(user_id, network_name).await? {
            let core = change.apply_to(&intent_core(&intent));
            intent.network_name = core.network_name;
            if let Some(secret) = core.network_secret {
                intent.network_secret = secret;
            }
            intent.secure_mode = core.secure_mode;
            intent.disable_encryption = core.disable_encryption;
            intent.encryption_algorithm = core.encryption_algorithm;
            if let Some(display_name) = core.display_name {
                intent.display_name = display_name;
            }
            // Temporary devices with their own addresses move along and keep
            // their host parts; on DHCP they follow on their own.
            if let Some(subnet) = change.virtual_subnet.as_deref().and_then(Ipv4Inet::parse)
                && intent.virtual_cidr.is_some()
            {
                let subnet = subnet.network();
                intent.virtual_cidr = Some(subnet.to_string());
                let moved = |address: &mut Option<String>| {
                    *address = address
                        .as_deref()
                        .and_then(|address| address.parse().ok())
                        .and_then(|address| subnet.host(address))
                        .map(|address| address.to_string());
                };
                for member in &mut intent.members {
                    moved(&mut member.virtual_ipv4);
                    moved(&mut member.allocated_ipv4);
                }
            }
            self.persist(intent).await?;
        }
        let before = mesh_network::current_core(&self.db, user_id, network_name)
            .await
            .map_err(invalid)?;
        let results = core_sync::apply(
            &self.db,
            &self.client_manager,
            user_id,
            network_name,
            &change,
        )
        .await
        .map_err(invalid)?;
        if let Some((before, _)) = before {
            mesh_network::record_core(
                &self.db,
                user_id,
                network_name,
                &change.apply_to(&before),
                core_sync::now_ms() as i64,
            )
            .await
            .map_err(invalid)?;
        }
        Ok(results)
    }

    pub(crate) fn gateway_peer(&self) -> Option<&str> {
        self.gateway_config().map(|config| config.peer_url.as_str())
    }

    /// Networks the console keeps a record of, members or not.
    pub async fn list_mesh_networks(
        &self,
        user_id: UserIdInDb,
    ) -> Result<Vec<MeshNetworkView>, CentralNetworkServiceError> {
        mesh_network::list(&self.db, user_id).await.map_err(invalid)
    }

    pub async fn get_mesh_network(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
    ) -> Result<MeshNetworkView, CentralNetworkServiceError> {
        mesh_network::get(&self.db, user_id, network_name, self.gateway_peer())
            .await
            .map_err(invalid)?
            .ok_or_else(|| CentralNetworkServiceError::NotFound(format!("network {network_name}")))
    }

    pub async fn create_mesh_network(
        &self,
        user_id: UserIdInDb,
        request: CreateMeshNetworkReq,
    ) -> Result<Vec<JoinResult>, CentralNetworkServiceError> {
        let _mutation = self.lock_mutations(user_id).await;
        let network_name = request.network_name.trim().to_owned();
        let network_secret = match request.network_secret {
            Some(secret) if secret.is_empty() => String::new(),
            Some(secret) => normalize_network_secret(secret)?,
            None => random_secret(),
        };
        let core = CoreSettings {
            network_name: network_name.clone(),
            network_secret: Some(network_secret),
            secure_mode: request.secure_mode,
            display_name: request
                .display_name
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty()),
            virtual_ipv4: match request
                .virtual_subnet
                .as_deref()
                .map(str::trim)
                .filter(|subnet| !subnet.is_empty())
            {
                Some(subnet) => Some(
                    Ipv4Inet::parse(subnet)
                        .filter(|subnet| (1..=30).contains(&subnet.prefix))
                        .ok_or_else(|| {
                            CentralNetworkServiceError::Invalid(
                                "virtual subnet must look like 10.126.126.0/24".to_owned(),
                            )
                        })?
                        .network()
                        .to_string(),
                ),
                None => None,
            },
            ..Default::default()
        };
        let preset = request
            .preset
            .unwrap_or_else(|| Preset::new(self.gateway_peer()));
        mesh_network::create(&self.db, user_id, core, preset)
            .await
            .map_err(invalid)?;
        Ok(self
            .join_devices_locked(user_id, &network_name, &request.devices)
            .await)
    }

    async fn join_devices_locked(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
        devices: &[Uuid],
    ) -> Vec<JoinResult> {
        // Every join reads the record; make it before they run together.
        if let Err(error) =
            mesh_network::ensure_record(&self.db, user_id, network_name, self.gateway_peer()).await
        {
            tracing::debug!(user_id, network_name, %error, "joins report the missing network");
        }
        let results = futures::future::join_all(devices.iter().map(|device_id| {
            mesh_network::join(
                &self.db,
                &self.client_manager,
                user_id,
                network_name,
                *device_id,
                true,
                self.gateway_peer(),
            )
        }))
        .await;
        core_sync::observe_user(&self.db, &self.client_manager, user_id).await;
        results
    }

    pub async fn join_mesh_devices(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
        devices: &[Uuid],
    ) -> Result<Vec<JoinResult>, CentralNetworkServiceError> {
        let _mutation = self.lock_mutations(user_id).await;
        core_sync::observe_user(&self.db, &self.client_manager, user_id).await;
        Ok(self
            .join_devices_locked(user_id, network_name, devices)
            .await)
    }

    pub async fn save_mesh_preset(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
        preset: Preset,
    ) -> Result<MeshNetworkView, CentralNetworkServiceError> {
        {
            let _mutation = self.lock_mutations(user_id).await;
            core_sync::observe_user(&self.db, &self.client_manager, user_id).await;
            mesh_network::save_preset(&self.db, user_id, network_name, preset, self.gateway_peer())
                .await
                .map_err(invalid)?;
        }
        self.get_mesh_network(user_id, network_name).await
    }

    pub async fn set_mesh_auto_join(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
        enabled: bool,
    ) -> Result<MeshNetworkView, CentralNetworkServiceError> {
        {
            let _mutation = self.lock_mutations(user_id).await;
            core_sync::observe_user(&self.db, &self.client_manager, user_id).await;
            mesh_network::set_auto_join(
                &self.db,
                user_id,
                network_name,
                enabled,
                self.gateway_peer(),
            )
            .await
            .map_err(invalid)?;
        }
        self.get_mesh_network(user_id, network_name).await
    }

    pub async fn apply_mesh_advanced(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
        change: AdvancedChange,
    ) -> Result<Vec<MemberSyncResult>, CentralNetworkServiceError> {
        let _mutation = self.lock_mutations(user_id).await;
        mesh_network::apply_advanced(
            &self.db,
            &self.client_manager,
            user_id,
            network_name,
            &change,
        )
        .await
        .map_err(invalid)
    }

    /// The network's access control, rebuilt from what its members hold.
    pub async fn get_mesh_network_acl(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
    ) -> Result<NetworkAclView, CentralNetworkServiceError> {
        let members = self
            .db
            .list_mesh_observations(user_id, Some(network_name))
            .await?;
        let acl = mesh_network::acl_of(&members);
        let expected = acl.as_ref().map(network_acl::rules_of);
        let statuses = members
            .iter()
            .filter(|member| !member.credential)
            .map(|member| {
                let held: MemberAcl = serde_json::from_str(&member.acl).unwrap_or_default();
                MemberAclStatus {
                    device_id: member.device_id.clone(),
                    instance_id: member.instance_id.clone(),
                    in_sync: held.network == expected,
                    own_default: held.own_default,
                }
            })
            .collect();
        let mut temporary = Vec::new();
        let mut acl = acl.map(NetworkAcl::without_secrets);
        if let Some(intent) = self.load_by_name(user_id, network_name).await? {
            for member in &intent.members {
                let Some(credential) = member.credential_id.as_deref().and_then(|id| {
                    intent
                        .credentials
                        .iter()
                        .find(|credential| credential.id == id)
                }) else {
                    continue;
                };
                let group = member_group_name(member.id);
                if let Some(acl) = &mut acl {
                    for acl_group in &mut acl.groups {
                        if credential.grant.acl_groups.contains(&acl_group.name) {
                            acl_group.members.insert(group.clone());
                        }
                    }
                }
                temporary.push(TemporaryAclMember {
                    member_id: member.id,
                    device_id: member.device_id.clone(),
                    hostname: member.hostname.clone(),
                    group,
                });
            }
        }
        Ok(NetworkAclView {
            acl,
            members: statuses,
            temporary,
        })
    }

    /// Write the network's access control, or remove it with `None`, to
    /// every unlocked member as an advanced edit. Temporary devices join
    /// groups through their credential grants.
    pub async fn set_mesh_network_acl(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
        acl: Option<NetworkAcl>,
    ) -> Result<Vec<MemberSyncResult>, CentralNetworkServiceError> {
        let _mutation = self.lock_mutations(user_id).await;
        if let Some(acl) = &acl {
            acl.validate()
                .map_err(|error| CentralNetworkServiceError::Invalid(format!("{error:#}")))?;
        }
        core_sync::observe_user(&self.db, &self.client_manager, user_id).await;
        let current = mesh_network::current_acl(&self.db, user_id, network_name)
            .await
            .map_err(invalid)?;
        let acl = acl.map(|acl| {
            acl.normalized()
                .with_secrets(current.as_ref(), random_secret)
        });
        if let Some(mut intent) = self.load_by_name(user_id, network_name).await? {
            let mut changed = false;
            let members: Vec<_> = intent
                .members
                .iter()
                .filter_map(|member| Some((member.id, member.credential_id.clone()?)))
                .collect();
            for (member_id, credential_id) in members {
                let group = member_group_name(member_id);
                let mut groups: Vec<_> = acl
                    .iter()
                    .flat_map(|acl| &acl.groups)
                    .filter(|acl_group| acl_group.members.contains(&group))
                    .map(|acl_group| acl_group.name.clone())
                    .chain([group.clone()])
                    .collect();
                groups.sort();
                groups.dedup();
                if let Some(credential) = intent
                    .credentials
                    .iter_mut()
                    .find(|credential| credential.id == credential_id)
                {
                    let mut held = credential.grant.acl_groups.clone();
                    held.sort();
                    if held != groups {
                        credential.grant.acl_groups = groups;
                        changed = true;
                    }
                }
            }
            if changed {
                self.persist(intent).await?;
            }
        }
        let change = AdvancedChange::from([(
            advanced::NETWORK_ACL.to_owned(),
            serde_json::to_value(&acl)
                .map_err(|error| CentralNetworkServiceError::Invalid(error.to_string()))?,
        )]);
        mesh_network::apply_advanced(
            &self.db,
            &self.client_manager,
            user_id,
            network_name,
            &change,
        )
        .await
        .map_err(invalid)
    }

    pub async fn set_mesh_lock(
        &self,
        user_id: UserIdInDb,
        device_id: Uuid,
        instance_id: Uuid,
        locked: bool,
    ) -> Result<(), CentralNetworkServiceError> {
        self.db
            .set_mesh_lock(
                user_id,
                &device_id.to_string(),
                &instance_id.to_string(),
                locked,
            )
            .await?;
        Ok(())
    }

    /// Forget a network: the console's record, preset and temporary members
    /// go; the members keep their configurations and are paused.
    pub async fn delete_mesh_network(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
    ) -> Result<Vec<MemberSyncResult>, CentralNetworkServiceError> {
        let _mutation = self.lock_mutations(user_id).await;
        if let Some(intent) = self.load_by_name(user_id, network_name).await? {
            self.db
                .delete_central_network_intent(user_id, intent.id)
                .await?;
        }
        let results =
            mesh_network::pause_members(&self.db, &self.client_manager, user_id, network_name)
                .await
                .map_err(invalid)?;
        self.publish_user(user_id).await;
        Ok(results)
    }

    /// Instances this console has seen on its devices, for the network page.
    pub async fn mesh_members(
        &self,
        user_id: UserIdInDb,
    ) -> Result<Vec<MeshMemberInfo>, CentralNetworkServiceError> {
        let mut pending: std::collections::HashSet<_> = self
            .db
            .list_mesh_core_replays(user_id)
            .await?
            .into_iter()
            .map(|replay| (replay.device_id, replay.instance_id))
            .collect();
        pending.extend(
            self.db
                .list_mesh_advanced_replays(user_id)
                .await?
                .into_iter()
                .map(|replay| (replay.device_id, replay.instance_id)),
        );
        pending.extend(self.db.list_mesh_pause_replays(user_id).await?);
        let locked: std::collections::HashSet<_> = self
            .db
            .list_mesh_locks(user_id)
            .await?
            .into_iter()
            .collect();
        let online: std::collections::HashSet<_> = self
            .client_manager
            .list_sessions_by_user_id(user_id)
            .await
            .into_iter()
            .map(|token| token.machine_id.to_string())
            .collect();
        Ok(self
            .db
            .list_mesh_observations(user_id, None)
            .await?
            .into_iter()
            .map(|member| {
                let core = serde_json::from_str::<CoreSettings>(&member.core).ok();
                MeshMemberInfo {
                    online: online.contains(&member.device_id),
                    pending: pending
                        .contains(&(member.device_id.clone(), member.instance_id.clone())),
                    locked: locked
                        .contains(&(member.device_id.clone(), member.instance_id.clone())),
                    advanced: serde_json::from_str(&member.advanced).unwrap_or_default(),
                    virtual_ipv4: core.as_ref().and_then(|core| core.virtual_ipv4.clone()),
                    display_name: core.and_then(|core| core.display_name),
                    device_id: member.device_id,
                    instance_id: member.instance_id,
                    network_name: member.network_name,
                    sync_at: member.sync_at,
                    credential: member.credential,
                }
            })
            .collect())
    }

    async fn load_by_name(
        &self,
        user_id: UserIdInDb,
        network_name: &str,
    ) -> Result<Option<CentralNetworkIntent>, CentralNetworkServiceError> {
        use crate::db::entity::networks;
        let Some(row) = networks::Entity::find()
            .filter(networks::Column::UserId.eq(user_id))
            .filter(networks::Column::NetworkName.eq(network_name))
            .one(self.db.orm_db())
            .await?
        else {
            return Ok(None);
        };
        let network_id = Uuid::parse_str(&row.id)
            .map_err(|error| CentralNetworkServiceError::Database(error.to_string()))?;
        self.load(user_id, network_id).await.map(Some)
    }
}
