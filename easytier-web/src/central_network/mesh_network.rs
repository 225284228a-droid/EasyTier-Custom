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

use std::collections::BTreeSet;

use easytier::{
    common::config::ConfigSource,
    proto::{
        api::manage::{NetworkConfig, NetworkingMethod, PatchPersistedConfigRequest},
        common::SecureModeConfig,
    },
};
use easytier_core::management::remote_client::Storage as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    advanced::{self, AdvancedChange, AdvancedValues},
    core_sync::{
        self, CoreSettings, DISPLAY_NAME_CAPABILITY, MemberSyncResult, MemberSyncStatus,
        SYNC_CAPABILITY,
    },
};
use crate::{
    client_manager::{ClientManager, session::local_configs::LocalConfigError},
    db::{Db, MeshAdvancedReplay, MeshNetworkRecord, MeshObservation, UserIdInDb},
};

pub(crate) const DEFAULT_LISTENERS: [&str; 3] = [
    "tcp://0.0.0.0:11010",
    "udp://0.0.0.0:11010",
    "wg://0.0.0.0:11011",
];

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
    chosen
        .map(|(core, sync_at)| Ok((serde_json::from_str(core)?, sync_at)))
        .transpose()
}

/// The console's record for `network_name`, created from what its members
/// hold when the console has none yet.
async fn ensure_record(
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
    Ok((config, mask))
}

/// Create the network's configuration on `device_id` from its current core
/// settings and preset. Existing members are left alone.
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
            let (config, _) = joined_config(instance_id, &core, &preset, None, |_| false)?;
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
        let (config, field_mask) =
            joined_config(instance_id, &core, &preset, Some(stamp), supports)?;
        let written = manager
            .patch_local_config(
                user_id,
                device_id,
                PatchPersistedConfigRequest {
                    inst_id: Some(instance_id.into()),
                    expected_revision: String::new(),
                    config: Some(config),
                    field_mask,
                    toml: None,
                },
            )
            .await?;
        if start && let Some(entry) = core_sync::instance_entry(&written, &instance_id.to_string())
        {
            manager
                .mutate_local_config_lifecycle(
                    user_id,
                    device_id,
                    instance_id,
                    entry.revision,
                    Some(true),
                )
                .await?;
        }
        Ok::<_, anyhow::Error>((MemberSyncStatus::Saved, Some(instance_id)))
    }
    .await;
    core_sync::observe_user(db, manager, user_id).await;
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
        let Some(entry) = core_sync::instance_entry(&view, &member.instance_id) else {
            results.push(sync_result(
                &member,
                MemberSyncStatus::Failed,
                Some("configuration no longer exists".to_owned()),
            ));
            continue;
        };
        let supports = |name: &str| {
            view.capabilities
                .iter()
                .any(|capability| capability == name)
        };
        let config = entry.config.as_ref().expect("ready entries carry a config");
        let Some((desired, field_mask)) = advanced::patch(config, change, supports, Some(stamp))?
        else {
            results.push(sync_result(&member, MemberSyncStatus::Unchanged, None));
            continue;
        };
        let written = manager
            .patch_local_config(
                user_id,
                device_id,
                PatchPersistedConfigRequest {
                    inst_id: entry.inst_id,
                    expected_revision: entry.revision.clone(),
                    config: Some(desired),
                    field_mask,
                    toml: None,
                },
            )
            .await;
        results.push(match written {
            Ok(_) => sync_result(&member, MemberSyncStatus::Saved, None),
            Err(LocalConfigError::Conflict(_)) => {
                sync_result(&member, MemberSyncStatus::Conflict, None)
            }
            Err(error) => sync_result(&member, MemberSyncStatus::Failed, Some(error.to_string())),
        });
    }
    core_sync::observe_user(db, manager, user_id).await;
    Ok(results)
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
        let Some(entry) = core_sync::instance_entry(&view, &replay.instance_id) else {
            continue;
        };
        let config = entry.config.as_ref().expect("ready entries carry a config");
        let target = replay.target_sync_at as u64;
        // Edits on the node are overwritten; another console's later edit is kept.
        if !core_sync::replay_applies(config.advanced_sync_at, target) {
            continue;
        }
        let change: AdvancedChange = serde_json::from_str(&replay.change)?;
        let supports = |name: &str| {
            view.capabilities
                .iter()
                .any(|capability| capability == name)
        };
        let Some((desired, field_mask)) = advanced::patch(config, &change, supports, Some(target))?
        else {
            continue;
        };
        if let Err(error) = manager
            .patch_local_config(
                user_id,
                device_id,
                PatchPersistedConfigRequest {
                    inst_id: entry.inst_id,
                    expected_revision: entry.revision.clone(),
                    config: Some(desired),
                    field_mask,
                    toml: None,
                },
            )
            .await
        {
            tracing::warn!(user_id, %device_id, instance_id = replay.instance_id, %error, "missed advanced settings were not applied");
        }
    }
    Ok(())
}

async fn replay_pauses(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
) -> anyhow::Result<()> {
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
        if let Some(entry) = core_sync::instance_entry(&view, &instance)
            && entry.enabled
            && let Err(error) = pause_entry(manager, user_id, device_id, &entry).await
        {
            tracing::warn!(user_id, %device_id, instance, %error, "missed pause was not applied");
        }
    }
    Ok(())
}

async fn pause_entry(
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
    entry: &easytier::proto::api::manage::PersistedConfigEntry,
) -> Result<(), LocalConfigError> {
    let instance = entry
        .inst_id
        .map(Uuid::from)
        .ok_or_else(|| LocalConfigError::Invalid("configuration has no identity".to_owned()))?;
    manager
        .mutate_local_config_lifecycle(
            user_id,
            device_id,
            instance,
            entry.revision.clone(),
            Some(false),
        )
        .await
        .map(|_| ())
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
        let status = match core_sync::instance_entry(&view, &member.instance_id) {
            Some(entry) if entry.enabled => {
                match pause_entry(manager, user_id, device_id, &entry).await {
                    Ok(()) => sync_result(&member, MemberSyncStatus::Saved, None),
                    Err(error) => {
                        sync_result(&member, MemberSyncStatus::Failed, Some(error.to_string()))
                    }
                }
            }
            _ => sync_result(&member, MemberSyncStatus::Unchanged, None),
        };
        results.push(status);
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
        let (config, mask) = joined_config(id, &core, &preset, Some(7), |_| true).unwrap();
        assert_eq!(config.network_name.as_deref(), Some("office"));
        assert_eq!(config.instance_name.as_deref(), Some("Head office"));
        assert!(config.secure_mode.unwrap().enabled);
        assert_eq!(config.peer_urls, preset.peers);
        assert_eq!(config.dhcp, Some(true));
        assert_eq!(config.network_sync_at, Some(7));
        assert_eq!(config.latency_first, Some(true));
        assert!(mask.contains(&"latency_first".to_owned()));
        assert!(config.virtual_ipv4.is_none() && config.hostname.is_none());

        let (official, mask) = joined_config(id, &core, &preset, Some(7), |_| false).unwrap();
        assert!(official.instance_name.is_none() && official.network_sync_at.is_none());
        assert!(!mask.contains(&"instance_name".to_owned()));
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
