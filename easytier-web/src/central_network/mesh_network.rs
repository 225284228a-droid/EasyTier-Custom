//! A network as this console manages it. The network itself lives in the
//! configurations of its members; the console only keeps what no member
//! holds: the preset a joining device receives, whether new devices join on
//! their own, and the core settings it last wrote, so a network can exist
//! before its first member does.
//!
//! The preset (initial nodes, listeners and advanced settings) only reaches
//! devices when they join; it never changes an existing member. Advanced
//! settings of existing members are batch-edited incrementally: only the
//! settings the user touched are written, locked members are skipped, and a
//! member that was offline receives the edit once when it next connects,
//! unless it changed those settings itself in the meantime.
//!
//! The network's access control is rebuilt from its members in the same way
//! and written with the advanced settings' stamp; joining devices receive it.

use std::collections::{BTreeSet, HashMap};

use easytier::{
    common::config::ConfigSource,
    proto::{
        api::manage::{
            NetworkConfig, NetworkingMethod, PatchPersistedConfigRequest, PersistedConfigEntry,
        },
        common::SecureModeConfig,
    },
};
use easytier_core::management::remote_client::Storage as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    advanced::{self, AdvancedChange, AdvancedValues},
    core_sync::{
        self, CoreSettings, DISPLAY_NAME_CAPABILITY, Ipv4Inet, MemberSyncResult, MemberSyncStatus,
        SYNC_CAPABILITY,
    },
    delivery,
    network_acl::{self, MemberAcl, NetworkAcl},
};
use crate::{
    client_manager::{
        ClientManager,
        session::local_configs::{LocalConfigError, LocalConfigView},
    },
    db::{Db, MeshAdvancedReplay, MeshNetworkRecord, MeshObservation, UserIdInDb},
};

pub(crate) const DEFAULT_LISTENERS: [&str; 2] = ["tcp://0.0.0.0:11010", "udp://0.0.0.0:11010"];

/// What a device receives when it joins, besides the core settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    #[serde(default)]
    pub peers: Vec<String>,
    #[serde(default)]
    pub listeners: Vec<String>,
    #[serde(default)]
    pub advanced: AdvancedValues,
}

impl Preset {
    /// New devices reach the console's gateway first, when it has one.
    pub fn new(gateway_peer: Option<&str>) -> Self {
        Self {
            peers: gateway_peer.map(str::to_owned).into_iter().collect(),
            listeners: DEFAULT_LISTENERS.map(str::to_owned).to_vec(),
            advanced: AdvancedValues::new(),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        for url in self.peers.iter().chain(&self.listeners) {
            url.parse::<url::Url>()
                .map_err(|error| anyhow::anyhow!("invalid address {url}: {error}"))?;
        }
        advanced::validate(&self.advanced)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MeshNetworkView {
    pub network_name: String,
    pub core: CoreSettings,
    pub preset: Preset,
    pub auto_join: bool,
}

fn view_of(record: &MeshNetworkRecord, core: CoreSettings) -> anyhow::Result<MeshNetworkView> {
    Ok(MeshNetworkView {
        network_name: record.network_name.clone(),
        core,
        preset: serde_json::from_str(&record.preset)?,
        auto_join: record.auto_join_since.is_some(),
    })
}

fn permanent(members: Vec<MeshObservation>) -> Vec<MeshObservation> {
    members
        .into_iter()
        .filter(|member| !member.credential)
        .collect()
}

/// The newest core settings this console knows for `network_name`: its own
/// last write, or a member's when another console wrote later.
pub(crate) async fn current_core(
    db: &Db,
    user_id: UserIdInDb,
    network_name: &str,
) -> anyhow::Result<Option<(CoreSettings, Option<i64>)>> {
    let record = db.get_mesh_network(user_id, network_name).await?;
    let members = permanent(
        db.list_mesh_observations(user_id, Some(network_name))
            .await?,
    );
    let newest = members
        .iter()
        .max_by_key(|member| member.sync_at)
        .map(|member| (member.core.as_str(), member.sync_at));
    let chosen = match (&record, newest) {
        (Some(record), Some((core, sync_at))) if sync_at > record.sync_at => Some((core, sync_at)),
        (Some(record), _) => Some((record.core.as_str(), record.sync_at)),
        (None, newest) => newest,
    };
    let Some((core, sync_at)) = chosen else {
        return Ok(None);
    };
    let mut core: CoreSettings = serde_json::from_str(core)?;
    // The network's subnet: the one most static members use, else the one
    // most DHCP members are configured with, else the last one this console
    // wrote. None means the default.
    let cores: Vec<CoreSettings> = members
        .iter()
        .filter_map(|member| serde_json::from_str(&member.core).ok())
        .collect();
    let subnet = most_common(cores.iter().filter_map(CoreSettings::subnet))
        .or_else(|| most_common(cores.iter().filter_map(CoreSettings::dhcp_subnet)))
        .or_else(|| core.subnet());
    core.virtual_ipv4 = subnet.map(|subnet| subnet.to_string());
    Ok(Some((core, sync_at)))
}

/// The network's access control as its permanent members hold it.
pub(crate) fn acl_of(members: &[MeshObservation]) -> Option<NetworkAcl> {
    let parsed: Vec<_> = members
        .iter()
        .filter(|member| !member.credential)
        .map(|member| {
            (
                member,
                serde_json::from_str::<MemberAcl>(&member.acl).unwrap_or_default(),
            )
        })
        .collect();
    network_acl::current(
        parsed
            .iter()
            .map(|(member, acl)| (member.instance_id.as_str(), member.advanced_sync_at, acl)),
    )
}

pub(crate) async fn current_acl(
    db: &Db,
    user_id: UserIdInDb,
    network_name: &str,
) -> anyhow::Result<Option<NetworkAcl>> {
    Ok(acl_of(
        &db.list_mesh_observations(user_id, Some(network_name))
            .await?,
    ))
}

fn most_common(subnets: impl Iterator<Item = Ipv4Inet>) -> Option<Ipv4Inet> {
    let mut counts = HashMap::new();
    for subnet in subnets {
        *counts.entry(subnet.to_string()).or_insert(0usize) += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .and_then(|(subnet, _)| Ipv4Inet::parse(&subnet))
}

/// The console's record for `network_name`, created from what its members
/// hold when the console has none yet.
pub(crate) async fn ensure_record(
    db: &Db,
    user_id: UserIdInDb,
    network_name: &str,
    gateway_peer: Option<&str>,
) -> anyhow::Result<MeshNetworkRecord> {
    if let Some(record) = db.get_mesh_network(user_id, network_name).await? {
        return Ok(record);
    }
    let (core, sync_at) = current_core(db, user_id, network_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("network {network_name} does not exist"))?;
    let record = MeshNetworkRecord {
        network_name: network_name.to_owned(),
        core: serde_json::to_string(&core)?,
        sync_at,
        preset: serde_json::to_string(&Preset::new(gateway_peer))?,
        auto_join_since: None,
    };
    db.put_mesh_network(user_id, &record).await?;
    Ok(record)
}

pub(crate) async fn list(db: &Db, user_id: UserIdInDb) -> anyhow::Result<Vec<MeshNetworkView>> {
    let mut views = Vec::new();
    for record in db.list_mesh_networks(user_id).await? {
        let core = current_core(db, user_id, &record.network_name)
            .await?
            .map(|(core, _)| core)
            .unwrap_or_default();
        views.push(view_of(&record, core)?);
    }
    Ok(views)
}

/// The network as the console sees it, with a default preset when it keeps
/// no record of its own.
pub(crate) async fn get(
    db: &Db,
    user_id: UserIdInDb,
    network_name: &str,
    gateway_peer: Option<&str>,
) -> anyhow::Result<Option<MeshNetworkView>> {
    let Some((core, sync_at)) = current_core(db, user_id, network_name).await? else {
        return Ok(None);
    };
    let record = match db.get_mesh_network(user_id, network_name).await? {
        Some(record) => record,
        None => MeshNetworkRecord {
            network_name: network_name.to_owned(),
            core: serde_json::to_string(&core)?,
            sync_at,
            preset: serde_json::to_string(&Preset::new(gateway_peer))?,
            auto_join_since: None,
        },
    };
    view_of(&record, core).map(Some)
}

pub(crate) async fn create(
    db: &Db,
    user_id: UserIdInDb,
    core: CoreSettings,
    preset: Preset,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !core.network_name.trim().is_empty(),
        "network name must not be empty"
    );
    preset.validate()?;
    if current_core(db, user_id, &core.network_name)
        .await?
        .is_some()
    {
        anyhow::bail!("network {} already exists", core.network_name);
    }
    db.put_mesh_network(
        user_id,
        &MeshNetworkRecord {
            network_name: core.network_name.clone(),
            core: serde_json::to_string(&core)?,
            sync_at: Some(core_sync::next_stamp([]) as i64),
            preset: serde_json::to_string(&preset)?,
            auto_join_since: None,
        },
    )
    .await?;
    Ok(())
}

pub(crate) async fn save_preset(
    db: &Db,
    user_id: UserIdInDb,
    network_name: &str,
    preset: Preset,
    gateway_peer: Option<&str>,
) -> anyhow::Result<()> {
    preset.validate()?;
    let mut record = ensure_record(db, user_id, network_name, gateway_peer).await?;
    record.preset = serde_json::to_string(&preset)?;
    db.put_mesh_network(user_id, &record).await?;
    Ok(())
}

/// Switch joining on their own for devices that connect for the first time
/// from now on; the devices already known never join on their own.
pub(crate) async fn set_auto_join(
    db: &Db,
    user_id: UserIdInDb,
    network_name: &str,
    enabled: bool,
    gateway_peer: Option<&str>,
) -> anyhow::Result<()> {
    let mut record = ensure_record(db, user_id, network_name, gateway_peer).await?;
    if enabled == record.auto_join_since.is_some() {
        return Ok(());
    }
    if enabled {
        let known: Vec<_> = db
            .list_user_device_ids(user_id)
            .await?
            .iter()
            .map(Uuid::to_string)
            .collect();
        db.put_mesh_auto_joins(user_id, network_name, &known)
            .await?;
    }
    record.auto_join_since = enabled.then(|| core_sync::now_ms() as i64);
    db.put_mesh_network(user_id, &record).await?;
    Ok(())
}

/// Record the core settings this console just wrote to a network.
pub(crate) async fn record_core(
    db: &Db,
    user_id: UserIdInDb,
    network_name: &str,
    core: &CoreSettings,
    sync_at: i64,
) -> anyhow::Result<()> {
    if core.network_name != network_name {
        db.rename_mesh_network(user_id, network_name, &core.network_name)
            .await?;
    }
    if let Some(mut record) = db.get_mesh_network(user_id, &core.network_name).await? {
        record.core = serde_json::to_string(core)?;
        record.sync_at = Some(sync_at);
        db.put_mesh_network(user_id, &record).await?;
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct JoinResult {
    pub device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    pub status: MemberSyncStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The configuration a joining device receives and the fields it sets.
fn joined_config(
    instance_id: Uuid,
    core: &CoreSettings,
    preset: &Preset,
    acl: Option<&NetworkAcl>,
    stamp: Option<u64>,
    supports: impl Fn(&str) -> bool,
) -> anyhow::Result<(NetworkConfig, Vec<String>)> {
    let mut config = NetworkConfig {
        instance_id: Some(instance_id.to_string()),
        network_name: Some(core.network_name.clone()),
        network_secret: core.network_secret.clone(),
        secure_mode: core.secure_mode.then(|| SecureModeConfig {
            enabled: true,
            ..Default::default()
        }),
        disable_encryption: core.disable_encryption.then_some(true),
        encryption_algorithm: core.encryption_algorithm.clone(),
        networking_method: Some(if preset.peers.is_empty() {
            NetworkingMethod::Standalone
        } else {
            NetworkingMethod::Manual
        } as i32),
        peer_urls: preset.peers.clone(),
        listener_urls: preset.listeners.clone(),
        // Addresses are per device: joining devices take one from DHCP.
        dhcp: Some(true),
        ..Default::default()
    };
    let mut mask: Vec<String> = [
        "network_name",
        "network_secret",
        "secure_mode",
        "disable_encryption",
        "encryption_algorithm",
        "networking_method",
        "peer_urls",
        "listener_urls",
        "dhcp",
    ]
    .map(str::to_owned)
    .to_vec();
    // The network's subnet is where a joining device starts before it sees a peer.
    if let Some(subnet) = core.subnet() {
        config.dhcp_ipv4_cidr = Some(subnet.to_string());
        mask.push("dhcp_ipv4_cidr".to_owned());
    }
    if supports(DISPLAY_NAME_CAPABILITY) && core.display_name.is_some() {
        config.instance_name = core.display_name.clone();
        mask.push("instance_name".to_owned());
    }
    if let Some(stamp) = stamp.filter(|_| supports(SYNC_CAPABILITY)) {
        config.network_sync_at = Some(stamp);
        mask.push("network_sync_at".to_owned());
    }
    mask.extend(advanced::apply_to(
        &mut config,
        &preset.advanced,
        &supports,
    )?);
    if acl.is_some() {
        network_acl::apply(&mut config, acl);
        mask.push("acl".to_owned());
    }
    Ok((config, mask))
}

/// Create the network's configuration on `device_id` from its current core
/// settings and preset. Existing members are left alone. The caller observes
/// the devices afterwards.
pub(crate) async fn join(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    network_name: &str,
    device_id: Uuid,
    start: bool,
    gateway_peer: Option<&str>,
) -> JoinResult {
    let result = |status, instance_id: Option<Uuid>, message: Option<String>| JoinResult {
        device_id: device_id.to_string(),
        instance_id: instance_id.map(|id| id.to_string()),
        status,
        message,
    };
    let outcome = async {
        let record = ensure_record(db, user_id, network_name, gateway_peer).await?;
        let preset: Preset = serde_json::from_str(&record.preset)?;
        let (core, _) = current_core(db, user_id, network_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("network {network_name} does not exist"))?;
        let members = db
            .list_mesh_observations(user_id, Some(network_name))
            .await?;
        let acl = acl_of(&members);
        if let Some(existing) = members
            .iter()
            .find(|member| member.device_id == device_id.to_string())
        {
            return Ok((
                MemberSyncStatus::Unchanged,
                Uuid::parse_str(&existing.instance_id).ok(),
            ));
        }
        let stamp = core_sync::next_stamp(
            members
                .iter()
                .filter_map(|member| member.sync_at)
                .chain(record.sync_at)
                .map(|stamp| stamp as u64),
        );
        let instance_id = Uuid::new_v4();
        let online = manager
            .get_session_by_machine_id(user_id, &device_id)
            .is_some();
        if !manager.supports_local_configs(&(user_id, device_id)).await {
            // Official nodes keep no capability-gated fields.
            let (config, _) =
                joined_config(instance_id, &core, &preset, acl.as_ref(), None, |_| false)?;
            db.insert_or_update_user_network_config(
                (user_id, device_id),
                instance_id,
                config,
                ConfigSource::Web,
            )
            .await?;
            db.update_network_config_state((user_id, device_id), instance_id, !start)
                .await?;
            manager
                .invalidate_applied_config_revision(user_id, device_id)
                .await;
            let status = if online {
                MemberSyncStatus::Saved
            } else {
                MemberSyncStatus::Pending
            };
            return Ok((status, Some(instance_id)));
        }
        let view = manager
            .local_configs(user_id, device_id)
            .await?
            .filter(|view| view.online && !view.stale)
            .ok_or_else(|| anyhow::anyhow!("device is offline"))?;
        let supports = |name: &str| {
            view.capabilities
                .iter()
                .any(|capability| capability == name)
        };
        let (config, field_mask) = joined_config(
            instance_id,
            &core,
            &preset,
            acl.as_ref(),
            Some(stamp),
            supports,
        )?;
        let instance = instance_id.to_string();
        // A creation whose outcome was lost shows up in the next read.
        delivery::patch(manager, user_id, device_id, Some(view), |view| {
            Ok(core_sync::instance_entry(view, &instance)
                .is_none()
                .then(|| PatchPersistedConfigRequest {
                    inst_id: Some(instance_id.into()),
                    expected_revision: String::new(),
                    config: Some(config.clone()),
                    field_mask: field_mask.clone(),
                    toml: None,
                }))
        })
        .await?;
        if start {
            delivery::set_enabled(manager, user_id, device_id, &instance, None, true).await?;
        }
        Ok::<_, anyhow::Error>((MemberSyncStatus::Saved, Some(instance_id)))
    }
    .await;
    match outcome {
        Ok((status, instance_id)) => result(status, instance_id, None),
        Err(error) => result(MemberSyncStatus::Failed, None, Some(error.to_string())),
    }
}

async fn locks(db: &Db, user_id: UserIdInDb) -> anyhow::Result<BTreeSet<(String, String)>> {
    Ok(db.list_mesh_locks(user_id).await?.into_iter().collect())
}

fn sync_result(
    member: &MeshObservation,
    status: MemberSyncStatus,
    message: Option<String>,
) -> MemberSyncResult {
    MemberSyncResult {
        device_id: member.device_id.clone(),
        instance_id: member.instance_id.clone(),
        status,
        message,
    }
}

/// Write the touched advanced settings to every unlocked member.
pub(crate) async fn apply_advanced(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    network_name: &str,
    change: &AdvancedChange,
) -> anyhow::Result<Vec<MemberSyncResult>> {
    advanced::validate(change)?;
    anyhow::ensure!(!change.is_empty(), "no settings were changed");
    core_sync::observe_user(db, manager, user_id).await;
    let locked = locks(db, user_id).await?;
    let replays = db.list_mesh_advanced_replays(user_id).await?;
    let members = permanent(
        db.list_mesh_observations(user_id, Some(network_name))
            .await?,
    );
    let stamp = core_sync::next_stamp(
        members
            .iter()
            .filter_map(|member| member.advanced_sync_at)
            .chain(replays.iter().map(|replay| replay.target_sync_at))
            .map(|stamp| stamp as u64),
    );
    let mut results = Vec::new();
    // Online revision members, written together once all are known.
    let mut online = Vec::new();
    for member in members {
        if locked.contains(&(member.device_id.clone(), member.instance_id.clone())) {
            results.push(sync_result(&member, MemberSyncStatus::Locked, None));
            continue;
        }
        let device_id = Uuid::parse_str(&member.device_id)?;
        if !manager.supports_local_configs(&(user_id, device_id)).await {
            let online = manager
                .get_session_by_machine_id(user_id, &device_id)
                .is_some();
            let written = core_sync::rewrite_stored(
                db,
                manager,
                user_id,
                device_id,
                &member.instance_id,
                None,
                |config| {
                    Ok(advanced::patch(config, change, |_| false, None)?
                        .map(|(desired, _)| desired))
                },
            )
            .await;
            results.push(match written {
                Ok(true) if online => sync_result(&member, MemberSyncStatus::Saved, None),
                // Applied by the console when the node connects.
                Ok(true) => sync_result(&member, MemberSyncStatus::Pending, None),
                Ok(false) => sync_result(&member, MemberSyncStatus::Unchanged, None),
                Err(error) => {
                    sync_result(&member, MemberSyncStatus::Failed, Some(error.to_string()))
                }
            });
            continue;
        }
        let view = manager
            .local_configs(user_id, device_id)
            .await
            .ok()
            .flatten()
            .filter(|view| view.online && !view.stale);
        let Some(view) = view else {
            let earlier = replays.iter().find(|replay| {
                replay.device_id == member.device_id && replay.instance_id == member.instance_id
            });
            let combined = match earlier {
                Some(earlier) => advanced::then(&serde_json::from_str(&earlier.change)?, change),
                None => change.clone(),
            };
            db.put_mesh_advanced_replay(
                user_id,
                &MeshAdvancedReplay {
                    device_id: member.device_id.clone(),
                    instance_id: member.instance_id.clone(),
                    change: serde_json::to_string(&combined)?,
                    target_sync_at: stamp as i64,
                },
            )
            .await?;
            results.push(sync_result(&member, MemberSyncStatus::Pending, None));
            continue;
        };
        // A direct write supersedes an edit the member was still waiting for.
        db.delete_mesh_advanced_replay(user_id, &member.device_id, &member.instance_id)
            .await?;
        online.push((device_id, (view, member.instance_id.clone(), results.len())));
        results.push(sync_result(&member, MemberSyncStatus::Unchanged, None));
    }
    let written = delivery::per_device(online, |device_id, members| async move {
        let mut written = Vec::with_capacity(members.len());
        for (view, instance_id, index) in members {
            let outcome = delivery::patch(manager, user_id, device_id, Some(view), |view| {
                let entry = core_sync::instance_entry(view, &instance_id).ok_or_else(|| {
                    LocalConfigError::NotFound("configuration no longer exists".to_owned())
                })?;
                let config = entry.config.as_ref().expect("ready entries carry a config");
                // Another console's newer edit is kept.
                if config.advanced_sync_at.is_some_and(|seen| seen > stamp) {
                    return Err(LocalConfigError::Conflict(None));
                }
                advanced_request(view, &entry, change, stamp)
            })
            .await;
            written.push((index, outcome));
        }
        written
    })
    .await;
    for (index, outcome) in written {
        let (status, message) = match outcome {
            Ok(true) => (MemberSyncStatus::Saved, None),
            Ok(false) => (MemberSyncStatus::Unchanged, None),
            Err(LocalConfigError::Conflict(_)) => (MemberSyncStatus::Conflict, None),
            Err(error) => (MemberSyncStatus::Failed, Some(error.to_string())),
        };
        results[index].status = status;
        results[index].message = message;
    }
    core_sync::observe_user(db, manager, user_id).await;
    Ok(results)
}

/// The write that gives the entry the touched advanced settings, if any differ.
fn advanced_request(
    view: &LocalConfigView,
    entry: &PersistedConfigEntry,
    change: &AdvancedChange,
    stamp: u64,
) -> Result<Option<PatchPersistedConfigRequest>, LocalConfigError> {
    let supports = |name: &str| {
        view.capabilities
            .iter()
            .any(|capability| capability == name)
    };
    let config = entry.config.as_ref().expect("ready entries carry a config");
    let patch = advanced::patch(config, change, supports, Some(stamp))
        .map_err(|error| LocalConfigError::Invalid(error.to_string()))?;
    Ok(
        patch.map(|(desired, field_mask)| PatchPersistedConfigRequest {
            inst_id: entry.inst_id,
            expected_revision: entry.revision.clone(),
            config: Some(desired),
            field_mask,
            toml: None,
        }),
    )
}

/// Send each missed advanced edit and pause once to members that came back
/// online.
pub(crate) async fn replay_user(db: &Db, manager: &ClientManager, user_id: UserIdInDb) {
    if let Err(error) = replay_advanced(db, manager, user_id).await {
        tracing::warn!(user_id, %error, "missed advanced settings will be retried");
    }
    if let Err(error) = replay_pauses(db, manager, user_id).await {
        tracing::warn!(user_id, %error, "missed pauses will be retried");
    }
}

async fn replay_advanced(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
) -> anyhow::Result<()> {
    let locked = locks(db, user_id).await?;
    let mut due = Vec::new();
    for replay in db.list_mesh_advanced_replays(user_id).await? {
        let Ok(device_id) = Uuid::parse_str(&replay.device_id) else {
            continue;
        };
        let Some(view) = manager
            .local_configs(user_id, device_id)
            .await
            .ok()
            .flatten()
            .filter(|view| view.online && !view.stale)
        else {
            continue;
        };
        // Once: the attempt is consumed whatever happens below.
        db.delete_mesh_advanced_replay(user_id, &replay.device_id, &replay.instance_id)
            .await?;
        if locked.contains(&(replay.device_id.clone(), replay.instance_id.clone())) {
            continue;
        }
        let change: AdvancedChange = serde_json::from_str(&replay.change)?;
        due.push((device_id, (view, replay, change)));
    }
    delivery::per_device(due, |device_id, replays| async move {
        for (view, replay, change) in replays {
            let target = replay.target_sync_at as u64;
            let written = delivery::patch(manager, user_id, device_id, Some(view), |view| {
                let Some(entry) = core_sync::instance_entry(view, &replay.instance_id) else {
                    return Ok(None);
                };
                let config = entry.config.as_ref().expect("ready entries carry a config");
                // Edits on the node are overwritten; another console's later edit is kept.
                if !core_sync::replay_applies(config.advanced_sync_at, target) {
                    return Ok(None);
                }
                advanced_request(view, &entry, &change, target)
            })
            .await;
            if let Err(error) = written {
                tracing::warn!(user_id, %device_id, instance_id = replay.instance_id, %error, "missed advanced settings were not applied");
            }
        }
        Vec::<()>::new()
    })
    .await;
    Ok(())
}

async fn replay_pauses(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
) -> anyhow::Result<()> {
    let mut due = Vec::new();
    for (device, instance) in db.list_mesh_pause_replays(user_id).await? {
        let Ok(device_id) = Uuid::parse_str(&device) else {
            continue;
        };
        let Some(view) = manager
            .local_configs(user_id, device_id)
            .await
            .ok()
            .flatten()
            .filter(|view| view.online && !view.stale)
        else {
            continue;
        };
        db.set_mesh_pause_replay(user_id, &device, &instance, false)
            .await?;
        if core_sync::instance_entry(&view, &instance).is_some() {
            due.push((device_id, (view, instance)));
        }
    }
    delivery::per_device(due, |device_id, pauses| async move {
        for (view, instance) in pauses {
            if let Err(error) =
                delivery::set_enabled(manager, user_id, device_id, &instance, Some(view), false)
                    .await
            {
                tracing::warn!(user_id, %device_id, instance, %error, "missed pause was not applied");
            }
        }
        Vec::<()>::new()
    })
    .await;
    Ok(())
}

/// Join devices that connected for the first time since a network let new
/// devices join on their own. Each device is tried once.
pub(crate) async fn auto_join_user(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    gateway_peer: Option<&str>,
) {
    let result = async {
        let records = db.list_mesh_networks(user_id).await?;
        if records.iter().all(|record| record.auto_join_since.is_none()) {
            return Ok(());
        }
        let devices = db.list_user_device_ids(user_id).await?;
        for record in records.iter().filter(|record| record.auto_join_since.is_some()) {
            let joined: BTreeSet<_> = db
                .mesh_auto_joined(user_id, &record.network_name)
                .await?
                .into_iter()
                .collect();
            for device_id in devices
                .iter()
                .filter(|device| !joined.contains(&device.to_string()))
            {
                // A revision node joins once its catalog can be read.
                if manager.get_session_by_machine_id(user_id, device_id).is_none()
                    || (manager.supports_local_configs(&(user_id, *device_id)).await
                        && !manager
                            .local_configs(user_id, *device_id)
                            .await
                            .ok()
                            .flatten()
                            .is_some_and(|view| view.online && !view.stale))
                {
                    continue;
                }
                db.put_mesh_auto_joins(user_id, &record.network_name, &[device_id.to_string()])
                    .await?;
                let joined = join(
                    db,
                    manager,
                    user_id,
                    &record.network_name,
                    *device_id,
                    true,
                    gateway_peer,
                )
                .await;
                core_sync::observe_user(db, manager, user_id).await;
                tracing::info!(user_id, %device_id, network = record.network_name, status = ?joined.status, message = ?joined.message, "new device joined on its own");
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(user_id, %error, "devices will join on their own later");
    }
}

/// Forget the network and pause its members: online ones now, offline ones
/// once when they next connect. Locked members keep running. Their
/// configurations stay on the devices.
pub(crate) async fn pause_members(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    network_name: &str,
) -> anyhow::Result<Vec<MemberSyncResult>> {
    core_sync::observe_user(db, manager, user_id).await;
    let locked = locks(db, user_id).await?;
    let mut results = Vec::new();
    let mut online = Vec::new();
    for member in permanent(
        db.list_mesh_observations(user_id, Some(network_name))
            .await?,
    ) {
        db.delete_mesh_core_replay(user_id, &member.device_id, &member.instance_id)
            .await?;
        db.delete_mesh_advanced_replay(user_id, &member.device_id, &member.instance_id)
            .await?;
        if locked.contains(&(member.device_id.clone(), member.instance_id.clone())) {
            results.push(sync_result(&member, MemberSyncStatus::Locked, None));
            continue;
        }
        let device_id = Uuid::parse_str(&member.device_id)?;
        if !manager.supports_local_configs(&(user_id, device_id)).await {
            let paused = core_sync::rewrite_stored(
                db,
                manager,
                user_id,
                device_id,
                &member.instance_id,
                Some(true),
                |_| Ok(None),
            )
            .await;
            results.push(match paused {
                Ok(true) => sync_result(&member, MemberSyncStatus::Saved, None),
                Ok(false) => sync_result(&member, MemberSyncStatus::Unchanged, None),
                Err(error) => {
                    sync_result(&member, MemberSyncStatus::Failed, Some(error.to_string()))
                }
            });
            continue;
        }
        let view = manager
            .local_configs(user_id, device_id)
            .await
            .ok()
            .flatten()
            .filter(|view| view.online && !view.stale);
        let Some(view) = view else {
            db.set_mesh_pause_replay(user_id, &member.device_id, &member.instance_id, true)
                .await?;
            results.push(sync_result(&member, MemberSyncStatus::Pending, None));
            continue;
        };
        if core_sync::instance_entry(&view, &member.instance_id).is_some() {
            online.push((device_id, (view, member.instance_id.clone(), results.len())));
        }
        results.push(sync_result(&member, MemberSyncStatus::Unchanged, None));
    }
    let paused = delivery::per_device(online, |device_id, members| async move {
        let mut paused = Vec::with_capacity(members.len());
        for (view, instance_id, index) in members {
            let outcome =
                delivery::set_enabled(manager, user_id, device_id, &instance_id, Some(view), false)
                    .await;
            paused.push((index, outcome));
        }
        paused
    })
    .await;
    for (index, outcome) in paused {
        match outcome {
            Ok(true) => results[index].status = MemberSyncStatus::Saved,
            Ok(false) => {}
            Err(error) => {
                results[index].status = MemberSyncStatus::Failed;
                results[index].message = Some(error.to_string());
            }
        }
    }
    db.delete_mesh_network(user_id, network_name).await?;
    core_sync::observe_user(db, manager, user_id).await;
    Ok(results)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn joining_devices_get_core_settings_and_the_preset_only() {
        let core = CoreSettings {
            network_name: "office".into(),
            network_secret: Some("s1".into()),
            secure_mode: true,
            display_name: Some("Head office".into()),
            ..Default::default()
        };
        let preset = Preset {
            peers: vec!["tcp://gateway:11010".into()],
            listeners: vec!["tcp://0.0.0.0:11010".into()],
            advanced: [("latency_first".to_owned(), json!(true))].into(),
        };
        let id = Uuid::new_v4();
        let (config, mask) = joined_config(id, &core, &preset, None, Some(7), |_| true).unwrap();
        assert_eq!(config.network_name.as_deref(), Some("office"));
        assert_eq!(config.instance_name.as_deref(), Some("Head office"));
        assert!(config.secure_mode.unwrap().enabled);
        assert_eq!(config.peer_urls, preset.peers);
        assert_eq!(config.dhcp, Some(true));
        assert_eq!(config.network_sync_at, Some(7));
        assert_eq!(config.latency_first, Some(true));
        assert!(mask.contains(&"latency_first".to_owned()));
        assert!(config.virtual_ipv4.is_none() && config.hostname.is_none());

        let (official, mask) = joined_config(id, &core, &preset, None, Some(7), |_| false).unwrap();
        assert!(official.instance_name.is_none() && official.network_sync_at.is_none());
        assert!(!mask.contains(&"instance_name".to_owned()));
        assert!(
            config.dhcp_ipv4_cidr.is_none(),
            "no subnet leaves DHCP at its default"
        );

        // A network with a subnet starts joining devices there.
        let core = CoreSettings {
            virtual_ipv4: Some("10.20.30.0/24".into()),
            ..core
        };
        let (config, mask) = joined_config(id, &core, &preset, None, Some(7), |_| true).unwrap();
        assert_eq!(config.dhcp, Some(true));
        assert_eq!(config.dhcp_ipv4_cidr.as_deref(), Some("10.20.30.0/24"));
        assert!(mask.contains(&"dhcp_ipv4_cidr".to_owned()));
    }

    #[test]
    fn presets_reject_bad_addresses_and_device_settings() {
        let mut preset = Preset::new(Some("tcp://gateway:11010"));
        assert!(preset.validate().is_ok());
        preset.peers.push("not a url".into());
        assert!(preset.validate().is_err());
        let mut preset = Preset::new(None);
        preset.advanced.insert("no_tun".into(), json!(true));
        assert!(preset.validate().is_err());
    }
}
