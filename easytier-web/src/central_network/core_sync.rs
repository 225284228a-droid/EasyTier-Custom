//! Core settings are the few values every peer of a network must share to
//! stay connected: the network name, the secret, the secure mode switch and
//! encryption. The network's display name travels with them: it names each
//! member's configuration (`instance_name`) and never joins or splits peers.
//! So does its IPv4 subnet: moving it rewrites the network part of each
//! static member's address and keeps the host part. DHCP members keep the
//! subnet their peers advertise, so they are restarted to take the new one.
//!
//! A console writes a change to the members it can reach now. A member that
//! was offline receives the missed change once, the next time it connects:
//! it overwrites whatever the member holds, edits made on the node included,
//! unless another console wrote a newer change meanwhile. Later edits are
//! never reverted. Nothing else in a member's configuration is touched.
//!
//! Several consoles can manage one node. Each change carries a UTC timestamp
//! stored on the node (`network_sync_at`), so the newest change wins; an edit
//! on the node carries none and counts as older than any console's change. Official nodes connect to
//! one console, which stores their configurations itself: a change is
//! written over that copy and reaches the node whenever it is online.

use std::{
    fmt,
    net::Ipv4Addr,
    time::{SystemTime, UNIX_EPOCH},
};

use easytier::proto::{
    api::manage::{
        ManagedCredentialConfig, NetworkConfig, PatchPersistedConfigRequest, PersistedConfigEntry,
    },
    common::SecureModeConfig,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use easytier::common::config::ConfigSource;
use easytier_core::management::remote_client::{
    ListNetworkProps, PersistentConfig as _, Storage as _,
};

use super::delivery;
use crate::{
    client_manager::{
        ClientManager,
        session::local_configs::{LocalConfigError, LocalConfigView},
    },
    db::{Db, MeshCoreReplay, MeshObservation, UserIdInDb},
};

pub(crate) const SYNC_CAPABILITY: &str = "config:network-sync-v1";
pub(crate) const DISPLAY_NAME_CAPABILITY: &str = "config:instance-name-v1";

/// An IPv4 address with its prefix length, written `a.b.c.d/len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ipv4Inet {
    pub address: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Inet {
    pub fn parse(value: &str) -> Option<Self> {
        let (address, prefix) = value.trim().split_once('/')?;
        Some(Self {
            address: address.parse().ok()?,
            prefix: prefix.parse().ok().filter(|prefix| *prefix <= 32)?,
        })
    }

    fn mask(self) -> u32 {
        u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0)
    }

    pub fn network(self) -> Self {
        Self {
            address: Ipv4Addr::from(u32::from(self.address) & self.mask()),
            ..self
        }
    }

    /// `address` under this subnet: its host bits are kept, unless they make
    /// the subnet's network or broadcast address.
    pub fn host(self, address: Ipv4Addr) -> Option<Ipv4Addr> {
        let mask = self.mask();
        let host = u32::from(address) & !mask;
        (self.prefix >= 31 || (host != 0 && host != !mask))
            .then(|| Ipv4Addr::from((u32::from(self.address) & mask) | host))
    }
}

impl fmt::Display for Ipv4Inet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

fn uses_dhcp(config: &NetworkConfig) -> bool {
    config.dhcp.unwrap_or(config.dhcp_ipv4_cidr.is_some())
}

/// The address a member sets itself; DHCP members have none.
fn static_ipv4(config: &NetworkConfig) -> Option<Ipv4Inet> {
    if uses_dhcp(config) {
        return None;
    }
    Some(Ipv4Inet {
        address: config.virtual_ipv4.as_deref()?.parse().ok()?,
        prefix: u8::try_from(config.network_length.unwrap_or(24))
            .ok()
            .filter(|prefix| *prefix <= 32)?,
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreSettings {
    pub network_name: String,
    /// `None` is a credential identity; `Some("")` a passwordless network.
    pub network_secret: Option<String>,
    pub secure_mode: bool,
    pub disable_encryption: bool,
    pub encryption_algorithm: Option<String>,
    /// The configuration's shown name; unset shows the network name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// A static member's address with its prefix; the network's own settings
    /// hold its subnet. Either way its network part is the network's subnet.
    /// DHCP members have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_ipv4: Option<String>,
    /// A DHCP member's configured subnet; it only names where the member
    /// starts when no peer advertises one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dhcp_ipv4_cidr: Option<String>,
}

impl CoreSettings {
    pub fn of(config: &NetworkConfig) -> Self {
        Self {
            network_name: config.network_name.clone().unwrap_or_default(),
            network_secret: config.network_secret.clone(),
            secure_mode: config.secure_mode.as_ref().is_some_and(|mode| mode.enabled),
            disable_encryption: config.disable_encryption.unwrap_or(false),
            encryption_algorithm: config
                .encryption_algorithm
                .clone()
                .filter(|algorithm| !algorithm.is_empty()),
            display_name: config
                .instance_name
                .clone()
                .filter(|name| !name.trim().is_empty()),
            virtual_ipv4: static_ipv4(config).map(|inet| inet.to_string()),
            dhcp_ipv4_cidr: config
                .dhcp_ipv4_cidr
                .as_deref()
                .filter(|_| uses_dhcp(config))
                .and_then(Ipv4Inet::parse)
                .map(|cidr| cidr.network().to_string()),
        }
    }

    /// The network part of its address, e.g. `10.126.126.0/24`.
    pub fn subnet(&self) -> Option<Ipv4Inet> {
        self.virtual_ipv4
            .as_deref()
            .and_then(Ipv4Inet::parse)
            .map(Ipv4Inet::network)
    }

    /// The configured subnet of a DHCP member.
    pub fn dhcp_subnet(&self) -> Option<Ipv4Inet> {
        self.dhcp_ipv4_cidr
            .as_deref()
            .and_then(Ipv4Inet::parse)
            .map(Ipv4Inet::network)
    }
}

/// The core settings a user edited; the others keep each member's value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreChange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_encryption: Option<bool>,
    /// An empty string selects the default algorithm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_algorithm: Option<String>,
    /// An empty string shows the network name again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The network's IPv4 subnet, e.g. `10.20.30.0/24`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_subnet: Option<String>,
}

impl CoreChange {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn apply_to(&self, base: &CoreSettings) -> CoreSettings {
        let mut settings = base.clone();
        if let Some(name) = &self.network_name {
            settings.network_name = name.clone();
        }
        if let Some(secret) = &self.network_secret {
            settings.network_secret = Some(secret.clone());
        }
        if let Some(secure_mode) = self.secure_mode {
            settings.secure_mode = secure_mode;
        }
        if let Some(disabled) = self.disable_encryption {
            settings.disable_encryption = disabled;
        }
        if let Some(algorithm) = &self.encryption_algorithm {
            settings.encryption_algorithm = (!algorithm.is_empty()).then(|| algorithm.clone());
        }
        if let Some(name) = &self.display_name {
            let name = name.trim();
            settings.display_name = (!name.is_empty()).then(|| name.to_owned());
        }
        if let Some(subnet) = self.virtual_subnet.as_deref().and_then(Ipv4Inet::parse) {
            let subnet = subnet.network();
            // A static member keeps its host part; anything else holds the subnet.
            let moved = base
                .virtual_ipv4
                .as_deref()
                .and_then(Ipv4Inet::parse)
                .and_then(|current| subnet.host(current.address))
                .map(|address| Ipv4Inet { address, ..subnet });
            settings.virtual_ipv4 = Some(moved.unwrap_or(subnet).to_string());
        }
        settings
    }

    /// A later change wins field by field over an earlier one.
    pub fn then(&self, later: &CoreChange) -> CoreChange {
        CoreChange {
            network_name: later.network_name.clone().or(self.network_name.clone()),
            network_secret: later.network_secret.clone().or(self.network_secret.clone()),
            secure_mode: later.secure_mode.or(self.secure_mode),
            disable_encryption: later.disable_encryption.or(self.disable_encryption),
            encryption_algorithm: later
                .encryption_algorithm
                .clone()
                .or(self.encryption_algorithm.clone()),
            display_name: later.display_name.clone().or(self.display_name.clone()),
            virtual_subnet: later.virtual_subnet.clone().or(self.virtual_subnet.clone()),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if let Some(name) = &self.network_name {
            anyhow::ensure!(!name.trim().is_empty(), "network name must not be empty");
        }
        if let Some(secret) = &self.network_secret {
            anyhow::ensure!(
                secret.is_empty() || !secret.trim().is_empty(),
                "network secret must not consist only of whitespace"
            );
        }
        if let Some(subnet) = &self.virtual_subnet {
            anyhow::ensure!(
                Ipv4Inet::parse(subnet).is_some_and(|subnet| (1..=30).contains(&subnet.prefix)),
                "virtual subnet must look like 10.126.126.0/24"
            );
        }
        Ok(())
    }
}

/// A temporary member authenticates with a credential instead of the secret.
pub(super) fn is_credential_member(config: &NetworkConfig) -> bool {
    config.network_secret.is_none()
        && config
            .secure_mode
            .as_ref()
            .is_some_and(|mode| mode.enabled && mode.local_private_key.is_some())
}

/// The write that gives `config` the `target` core settings, if any differ.
fn core_patch(
    config: &NetworkConfig,
    target: &CoreSettings,
    stamp: Option<u64>,
    display_name: bool,
) -> Option<(NetworkConfig, Vec<String>)> {
    let current = CoreSettings::of(config);
    let mut desired = config.clone();
    let mut mask = Vec::new();
    if current.network_name != target.network_name {
        desired.network_name = Some(target.network_name.clone());
        mask.push("network_name".to_owned());
    }
    if current.network_secret != target.network_secret {
        desired.network_secret = target.network_secret.clone();
        mask.push("network_secret".to_owned());
    }
    if current.secure_mode != target.secure_mode {
        // Only the switch is shared; the node keeps its own key pair.
        desired.secure_mode = target.secure_mode.then(|| SecureModeConfig {
            enabled: true,
            ..config.secure_mode.clone().unwrap_or_default()
        });
        mask.push("secure_mode".to_owned());
    }
    if current.disable_encryption != target.disable_encryption {
        desired.disable_encryption = target.disable_encryption.then_some(true);
        mask.push("disable_encryption".to_owned());
    }
    if current.encryption_algorithm != target.encryption_algorithm {
        desired.encryption_algorithm = target.encryption_algorithm.clone();
        mask.push("encryption_algorithm".to_owned());
    }
    if display_name && current.display_name != target.display_name {
        desired.instance_name = target.display_name.clone();
        mask.push("instance_name".to_owned());
    }
    // Only the subnet is shared: a static member keeps its host part, and a
    // DHCP member follows the others on its own.
    if let (Some(current), Some(subnet)) = (static_ipv4(config), target.subnet())
        && current.network() != subnet
        && let Some(address) = subnet.host(current.address)
    {
        desired.virtual_ipv4 = Some(address.to_string());
        desired.network_length = Some(subnet.prefix as i32);
        mask.push("virtual_ipv4".to_owned());
    }
    if mask.is_empty() {
        return None;
    }
    if let Some(stamp) = stamp {
        desired.network_sync_at = Some(stamp);
        mask.push("network_sync_at".to_owned());
    }
    Some((desired, mask))
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// UTC milliseconds that sort after every change already seen, even when
/// this console's clock runs behind the console that made it.
pub(super) fn next_stamp(seen: impl IntoIterator<Item = u64>) -> u64 {
    let now = now_ms();
    seen.into_iter()
        .fold(now, |stamp, seen| stamp.max(seen + 1))
}

pub(super) fn instance_entry(
    view: &LocalConfigView,
    instance_id: &str,
) -> Option<PersistedConfigEntry> {
    view.snapshot
        .entries
        .iter()
        .find(|entry| {
            entry.status == "ready"
                && entry.config.is_some()
                && entry
                    .inst_id
                    .map(Uuid::from)
                    .map(|id| id.to_string())
                    .as_deref()
                    == Some(instance_id)
        })
        .cloned()
}

fn observation(
    device_id: Uuid,
    instance_id: String,
    config: &NetworkConfig,
) -> anyhow::Result<MeshObservation> {
    let core = CoreSettings::of(config);
    Ok(MeshObservation {
        device_id: device_id.to_string(),
        instance_id,
        network_name: core.network_name.clone(),
        core: serde_json::to_string(&core)?,
        sync_at: config.network_sync_at.map(|stamp| stamp as i64),
        credential: is_credential_member(config),
        advanced: serde_json::to_string(&super::advanced::values_of(config))?,
        advanced_sync_at: config.advanced_sync_at.map(|stamp| stamp as i64),
        acl: serde_json::to_string(&super::network_acl::read(config.acl.as_ref()))?,
    })
}

fn observations_of(view: &LocalConfigView) -> anyhow::Result<Vec<MeshObservation>> {
    let mut observations = Vec::new();
    for entry in &view.snapshot.entries {
        let (Some(config), Some(instance_id)) = (&entry.config, entry.inst_id) else {
            continue;
        };
        if entry.status != "ready" {
            continue;
        }
        observations.push(observation(
            view.machine_id,
            Uuid::from(instance_id).to_string(),
            config,
        )?);
    }
    Ok(observations)
}

/// A configuration the console stores for a node without its own catalog.
pub(super) struct StoredConfig {
    pub instance_id: String,
    pub config: NetworkConfig,
    pub disabled: bool,
    pub source: ConfigSource,
}

pub(super) async fn stored_configs(
    db: &Db,
    user_id: UserIdInDb,
    device_id: Uuid,
) -> anyhow::Result<Vec<StoredConfig>> {
    db.list_network_configs((user_id, device_id), ListNetworkProps::All)
        .await?
        .into_iter()
        .map(|row| {
            Ok(StoredConfig {
                instance_id: row.get_network_inst_id().to_owned(),
                config: row.get_network_config()?,
                disabled: row.disabled,
                source: row.get_network_config_source(),
            })
        })
        .collect()
}

/// The WireGuard listener official consoles put in every new configuration.
const OFFICIAL_DEFAULT_WG_LISTENER: &str = "wg://0.0.0.0:11011";

/// What the console's copy of an official node's configuration must change so
/// the node runs like this fork's cores: official cores start with private
/// mode off when the configuration leaves it unset, this fork's with it on,
/// and official defaults listen on WireGuard, which this fork leaves opt-in.
/// Both happen once, when the console first takes the copy over: an unset
/// private mode marks a copy it has not touched yet, so a WireGuard listener
/// the user adds afterwards stays.
fn official_defaults(config: &NetworkConfig) -> Option<NetworkConfig> {
    if config.enable_private_mode.is_some() {
        return None;
    }
    Some(NetworkConfig {
        enable_private_mode: Some(true),
        listener_urls: config
            .listener_urls
            .iter()
            .filter(|url| url.as_str() != OFFICIAL_DEFAULT_WG_LISTENER)
            .cloned()
            .collect(),
        ..config.clone()
    })
}

/// Write the core settings over the console's copy of an official node's
/// configuration. The console applies its copy whenever the node is online,
/// so an offline node receives it on connection; nothing competes with it
/// because an official node connects to one console only.
async fn force_stored(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
    instance_id: &str,
    change: &CoreChange,
) -> anyhow::Result<bool> {
    rewrite_stored(
        db,
        manager,
        user_id,
        device_id,
        instance_id,
        None,
        |config| {
            let target = change.apply_to(&CoreSettings::of(config));
            // Official cores neither keep the stamp nor a separate display name.
            Ok(core_patch(config, &target, None, false).map(|(desired, _)| desired))
        },
    )
    .await
}

/// Rewrite the console's copy of an official node's configuration. The copy
/// keeps its running state unless `disabled` says otherwise.
pub(super) async fn rewrite_stored(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
    instance_id: &str,
    disabled: Option<bool>,
    rewrite: impl FnOnce(&NetworkConfig) -> anyhow::Result<Option<NetworkConfig>>,
) -> anyhow::Result<bool> {
    let stored = stored_configs(db, user_id, device_id)
        .await?
        .into_iter()
        .find(|stored| stored.instance_id == instance_id)
        .ok_or_else(|| anyhow::anyhow!("configuration no longer exists"))?;
    let desired = rewrite(&stored.config)?;
    let disabled_now = disabled.unwrap_or(stored.disabled);
    if desired.is_none() && disabled_now == stored.disabled {
        return Ok(false);
    }
    let instance = Uuid::parse_str(instance_id)?;
    if let Some(desired) = desired {
        db.insert_or_update_user_network_config(
            (user_id, device_id),
            instance,
            desired,
            stored.source,
        )
        .await?;
    }
    // Saving enables a configuration; a stopped network stays stopped.
    db.update_network_config_state((user_id, device_id), instance, disabled_now)
        .await?;
    manager
        .invalidate_applied_config_revision(user_id, device_id)
        .await;
    Ok(true)
}

/// Record the core settings of every instance this console can see:
/// revision-capable devices while online, and the stored copies of official
/// nodes. Offline revision-capable devices keep their last observation.
pub(crate) async fn observe_user(db: &Db, manager: &ClientManager, user_id: UserIdInDb) {
    let devices = match db.list_user_device_ids(user_id).await {
        Ok(devices) => devices,
        Err(error) => {
            tracing::warn!(user_id, %error, "cannot list devices for core sync");
            return;
        }
    };
    for device_id in devices {
        if !manager.supports_local_configs(&(user_id, device_id)).await {
            let result = async {
                for stored in stored_configs(db, user_id, device_id).await? {
                    if official_defaults(&stored.config).is_some() {
                        rewrite_stored(
                            db,
                            manager,
                            user_id,
                            device_id,
                            &stored.instance_id,
                            None,
                            |config| Ok(official_defaults(config)),
                        )
                        .await?;
                    }
                }
                let observations = stored_configs(db, user_id, device_id)
                    .await?
                    .iter()
                    .map(|stored| {
                        observation(device_id, stored.instance_id.clone(), &stored.config)
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                db.replace_mesh_observations(user_id, device_id, &observations)
                    .await
                    .map_err(anyhow::Error::from)
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(user_id, %device_id, %error, "cannot record stored configuration");
            }
            continue;
        }
        let view = match manager.local_configs(user_id, device_id).await {
            Ok(Some(view)) if view.online && !view.stale => view,
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!(user_id, %device_id, %error, "device configuration unavailable");
                continue;
            }
        };
        let result = match observations_of(&view) {
            Ok(observations) => db
                .replace_mesh_observations(user_id, device_id, &observations)
                .await
                .map_err(anyhow::Error::from),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            tracing::warn!(user_id, %device_id, %error, "cannot record device configuration");
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberSyncStatus {
    /// Written now.
    Saved,
    /// Already had these settings.
    Unchanged,
    /// Offline; the change is sent once when it next connects.
    Pending,
    /// Changed elsewhere since it was read; not written.
    Conflict,
    /// Locked against this console's batch edits; not written.
    Locked,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemberSyncResult {
    pub device_id: String,
    pub instance_id: String,
    pub status: MemberSyncStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The write that gives the entry the `target` core settings, if any differ.
fn core_request(
    view: &LocalConfigView,
    entry: &PersistedConfigEntry,
    target: &CoreSettings,
    stamp: u64,
) -> Option<PatchPersistedConfigRequest> {
    let config = entry.config.as_ref().expect("ready entries carry a config");
    let supports = |name: &str| {
        view.capabilities
            .iter()
            .any(|capability| capability == name)
    };
    let stamp = supports(SYNC_CAPABILITY).then_some(stamp);
    let (desired, mask) = core_patch(config, target, stamp, supports(DISPLAY_NAME_CAPABILITY))?;
    Some(PatchPersistedConfigRequest {
        inst_id: entry.inst_id,
        expected_revision: entry.revision.clone(),
        config: Some(desired),
        field_mask: mask,
        toml: None,
    })
}

fn no_longer_exists() -> LocalConfigError {
    LocalConfigError::NotFound("configuration no longer exists".to_owned())
}

/// Give one online member `change`, computed again from a fresh read before
/// each retry. Another console's newer change ends it as a conflict.
async fn write_change(
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
    view: LocalConfigView,
    instance_id: &str,
    change: &CoreChange,
    stamp: u64,
) -> Result<bool, LocalConfigError> {
    delivery::patch(manager, user_id, device_id, Some(view), |view| {
        let entry = instance_entry(view, instance_id).ok_or_else(no_longer_exists)?;
        let config = entry.config.as_ref().expect("ready entries carry a config");
        if config.network_sync_at.is_some_and(|seen| seen > stamp) {
            return Err(LocalConfigError::Conflict(None));
        }
        let target = change.apply_to(&CoreSettings::of(config));
        Ok(core_request(view, &entry, &target, stamp))
    })
    .await
}

fn sync_status(written: Result<bool, LocalConfigError>) -> (MemberSyncStatus, Option<String>) {
    match written {
        Ok(true) => (MemberSyncStatus::Saved, None),
        Ok(false) => (MemberSyncStatus::Unchanged, None),
        Err(LocalConfigError::Conflict(_)) => (MemberSyncStatus::Conflict, None),
        Err(error) => (MemberSyncStatus::Failed, Some(error.to_string())),
    }
}

/// Write `change` to every permanent member of `network_name` this console
/// knows. Offline members get a one-time replay; official nodes, which this
/// console cannot observe, are left to the caller.
pub(crate) async fn apply(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    network_name: &str,
    change: &CoreChange,
) -> anyhow::Result<Vec<MemberSyncResult>> {
    change.validate()?;
    observe_user(db, manager, user_id).await;
    let members: Vec<_> = db
        .list_mesh_observations(user_id, Some(network_name))
        .await?
        .into_iter()
        .filter(|member| !member.credential)
        .collect();
    let replays = db.list_mesh_core_replays(user_id).await?;
    let stamp = next_stamp(
        members
            .iter()
            .filter_map(|member| member.sync_at)
            .chain(replays.iter().map(|replay| replay.target_sync_at))
            .map(|stamp| stamp as u64),
    );
    let mut results = Vec::with_capacity(members.len());
    // Online members with a static address, and online DHCP members by result.
    let mut static_online = false;
    let mut dhcp_online = Vec::new();
    // Online revision members, written together once all are known.
    let mut online = Vec::new();
    for member in members {
        let device_id = Uuid::parse_str(&member.device_id)?;
        let result = |status, message: Option<String>| MemberSyncResult {
            device_id: member.device_id.clone(),
            instance_id: member.instance_id.clone(),
            status,
            message,
        };
        if !manager.supports_local_configs(&(user_id, device_id)).await {
            let online = manager
                .get_session_by_machine_id(user_id, &device_id)
                .is_some();
            static_online |= online
                && serde_json::from_str::<CoreSettings>(&member.core)
                    .is_ok_and(|core| core.virtual_ipv4.is_some());
            results.push(
                match force_stored(db, manager, user_id, device_id, &member.instance_id, change)
                    .await
                {
                    Ok(true) if online => result(MemberSyncStatus::Saved, None),
                    // Applied by the console when the node connects.
                    Ok(true) => result(MemberSyncStatus::Pending, None),
                    Ok(false) => result(MemberSyncStatus::Unchanged, None),
                    Err(error) => result(MemberSyncStatus::Failed, Some(error.to_string())),
                },
            );
            continue;
        }
        let view = manager
            .local_configs(user_id, device_id)
            .await
            .ok()
            .flatten();
        let Some(view) = view.filter(|view| view.online && !view.stale) else {
            let earlier = replays.iter().find(|replay| {
                replay.device_id == member.device_id && replay.instance_id == member.instance_id
            });
            // Several missed changes still replay once, as their combination
            // on top of what the member held before the first of them.
            let combined = match earlier {
                Some(earlier) => serde_json::from_str::<CoreChange>(&earlier.change)?.then(change),
                None => change.clone(),
            };
            db.put_mesh_core_replay(
                user_id,
                &MeshCoreReplay {
                    device_id: member.device_id.clone(),
                    instance_id: member.instance_id.clone(),
                    base: member.core.clone(),
                    base_sync_at: member.sync_at,
                    change: serde_json::to_string(&combined)?,
                    target_sync_at: stamp as i64,
                },
            )
            .await?;
            results.push(result(MemberSyncStatus::Pending, None));
            continue;
        };
        // A direct write supersedes a replay the member was still waiting for.
        db.delete_mesh_core_replay(user_id, &member.device_id, &member.instance_id)
            .await?;
        let Some(entry) = instance_entry(&view, &member.instance_id) else {
            results.push(result(
                MemberSyncStatus::Failed,
                Some("configuration no longer exists".to_owned()),
            ));
            continue;
        };
        let config = entry.config.as_ref().expect("ready entries carry a config");
        if static_ipv4(config).is_some() {
            static_online = true;
        } else if uses_dhcp(config) {
            dhcp_online.push((device_id, (member.instance_id.clone(), results.len())));
        }
        online.push((device_id, (view, member.instance_id.clone(), results.len())));
        results.push(result(MemberSyncStatus::Unchanged, None));
    }
    let written = delivery::per_device(online, |device_id, members| async move {
        let mut written = Vec::with_capacity(members.len());
        for (view, instance_id, index) in members {
            let outcome = write_change(
                manager,
                user_id,
                device_id,
                view,
                &instance_id,
                change,
                stamp,
            )
            .await;
            written.push((index, sync_status(outcome)));
        }
        written
    })
    .await;
    for (index, (status, message)) in written {
        results[index].status = status;
        results[index].message = message;
    }
    if let Some(subnet) = change.virtual_subnet.as_deref().and_then(Ipv4Inet::parse) {
        restart_dhcp_members(
            manager,
            user_id,
            subnet.network(),
            !static_online,
            dhcp_online,
            &mut results,
        )
        .await;
    }
    observe_user(db, manager, user_id).await;
    Ok(results)
}

/// DHCP members keep the subnet their peers advertise, so a moved network
/// restarts its running ones together: all are stopped first, then started
/// again, so they come back in the subnet the others now use. Without an
/// online static member, the first of them takes the subnet as its DHCP
/// subnet and starts first, so the network has a node there.
async fn restart_dhcp_members(
    manager: &ClientManager,
    user_id: UserIdInDb,
    subnet: Ipv4Inet,
    seed: bool,
    members: Vec<(Uuid, (String, usize))>,
    results: &mut [MemberSyncResult],
) {
    let mut running = Vec::new();
    for (device_id, (instance_id, index)) in members {
        if let Ok(Some(view)) = manager.local_configs(user_id, device_id).await
            && instance_entry(&view, &instance_id).is_some_and(|entry| entry.running)
        {
            running.push((device_id, (instance_id, index)));
        }
    }
    let mut failures = Vec::new();
    if seed && let Some((device_id, (instance_id, index))) = running.first() {
        let seeded = delivery::patch(manager, user_id, *device_id, None, |view| {
            let entry = instance_entry(view, instance_id).ok_or_else(no_longer_exists)?;
            let config = entry.config.as_ref().expect("ready entries carry a config");
            if config.dhcp_ipv4_cidr.as_deref() == Some(subnet.to_string().as_str()) {
                return Ok(None);
            }
            let mut desired = config.clone();
            desired.dhcp = Some(true);
            desired.dhcp_ipv4_cidr = Some(subnet.to_string());
            Ok(Some(PatchPersistedConfigRequest {
                inst_id: entry.inst_id,
                expected_revision: entry.revision.clone(),
                config: Some(desired),
                field_mask: vec!["dhcp_ipv4_cidr".to_owned()],
                toml: None,
            }))
        })
        .await;
        if let Err(error) = seeded {
            failures.push((*index, error));
        }
    }
    let toggle = |enabled: bool, members: Vec<(Uuid, (String, usize))>| {
        delivery::per_device(members, move |device_id, members| async move {
            let mut toggled = Vec::new();
            for (instance_id, index) in members {
                let outcome =
                    delivery::set_enabled(manager, user_id, device_id, &instance_id, None, enabled)
                        .await;
                toggled.push((instance_id, index, outcome));
            }
            toggled
        })
    };
    let mut stopped = Vec::new();
    for (instance_id, index, outcome) in toggle(false, running.clone()).await {
        match outcome {
            Ok(_) => stopped.push((instance_id, index)),
            Err(error) => failures.push((index, error)),
        }
    }
    let stopped: Vec<_> = running
        .into_iter()
        .filter(|(_, (instance_id, index))| {
            stopped
                .iter()
                .any(|(stopped, at)| stopped == instance_id && at == index)
        })
        .collect();
    // The seed comes back first; the others find it when they start.
    let (first, rest) = match seed {
        true => stopped.split_at(stopped.len().min(1)),
        false => stopped.split_at(0),
    };
    let mut started = toggle(true, first.to_vec()).await;
    started.extend(toggle(true, rest.to_vec()).await);
    for (_, index, outcome) in started {
        match outcome {
            Ok(_) if results[index].status == MemberSyncStatus::Unchanged => {
                results[index].status = MemberSyncStatus::Saved;
            }
            Ok(_) => {}
            Err(error) => failures.push((index, error)),
        }
    }
    for (index, error) in failures {
        results[index].status = MemberSyncStatus::Failed;
        results[index].message = Some(error.to_string());
    }
}

/// Whether a missed change is still sent: only another console's newer
/// change keeps it away. Edits on the node carry no stamp, so they are
/// overwritten like an older console's change.
pub(super) fn replay_applies(current_stamp: Option<u64>, target_stamp: u64) -> bool {
    current_stamp.is_none_or(|stamp| stamp <= target_stamp)
}

/// Send each missed change once to members that came back online.
pub(crate) async fn replay_user(db: &Db, manager: &ClientManager, user_id: UserIdInDb) {
    let replays = match db.list_mesh_core_replays(user_id).await {
        Ok(replays) => replays,
        Err(error) => {
            tracing::warn!(user_id, %error, "cannot read pending core changes");
            return;
        }
    };
    let mut due = Vec::new();
    for replay in replays {
        let Ok(device_id) = Uuid::parse_str(&replay.device_id) else {
            continue;
        };
        let view = match manager.local_configs(user_id, device_id).await {
            Ok(Some(view)) if view.online && !view.stale => view,
            _ => continue,
        };
        // Once: the attempt is consumed whatever happens below.
        if let Err(error) = db
            .delete_mesh_core_replay(user_id, &replay.device_id, &replay.instance_id)
            .await
        {
            tracing::warn!(user_id, %device_id, %error, "cannot consume pending core change");
            continue;
        }
        let Ok(change) = serde_json::from_str::<CoreChange>(&replay.change) else {
            continue;
        };
        let Some(entry) = instance_entry(&view, &replay.instance_id) else {
            continue;
        };
        let config = entry.config.as_ref().expect("ready entries carry a config");
        let target_name = change.apply_to(&CoreSettings::of(config)).network_name;
        // Catch up to the newest settings this console has seen for the
        // network, even when another console made that change.
        let newest = match db.list_mesh_observations(user_id, Some(&target_name)).await {
            Ok(peers) => peers
                .into_iter()
                .filter(|peer| !peer.credential && peer.instance_id != replay.instance_id)
                .filter(|peer| {
                    peer.sync_at
                        .is_some_and(|seen| seen > replay.target_sync_at)
                })
                .max_by_key(|peer| peer.sync_at)
                .and_then(|peer| {
                    let settings = serde_json::from_str::<CoreSettings>(&peer.core).ok()?;
                    Some((settings, peer.sync_at.unwrap_or_default() as u64))
                }),
            Err(_) => None,
        };
        due.push((device_id, (view, replay, change, newest)));
    }
    delivery::per_device(due, |device_id, replays| async move {
        for (view, replay, change, newest) in replays {
            let missed = replay.target_sync_at as u64;
            let written = delivery::patch(manager, user_id, device_id, Some(view), |view| {
                let Some(entry) = instance_entry(view, &replay.instance_id) else {
                    return Ok(None);
                };
                let config = entry.config.as_ref().expect("ready entries carry a config");
                if !replay_applies(config.network_sync_at, missed) {
                    tracing::info!(user_id, %device_id, instance_id = replay.instance_id, "another console changed the core settings later");
                    return Ok(None);
                }
                let mut target = change.apply_to(&CoreSettings::of(config));
                let mut stamp = missed;
                if let Some((settings, seen)) = &newest {
                    let mut settings = settings.clone();
                    // A DHCP member does not say which subnet the network uses.
                    if settings.virtual_ipv4.is_none() {
                        settings.virtual_ipv4 = target.virtual_ipv4.take();
                    }
                    target = settings;
                    stamp = *seen;
                }
                Ok(core_request(view, &entry, &target, stamp))
            })
            .await;
            if let Err(error) = written {
                tracing::warn!(user_id, %device_id, instance_id = replay.instance_id, %error, "missed core change was not applied");
            }
        }
        Vec::<()>::new()
    })
    .await;
    observe_user(db, manager, user_id).await;
}

/// Place a temporary-node network's credentials on its online permanent
/// members. Only the credentials this console placed earlier are replaced,
/// so another console's credentials survive.
pub(crate) async fn deliver_credentials(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    network_id: Uuid,
    network_name: &str,
    credentials: &[ManagedCredentialConfig],
) -> anyhow::Result<()> {
    if credentials.is_empty()
        && !db
            .has_mesh_credential_deliveries(user_id, network_id)
            .await?
    {
        return Ok(());
    }
    let ours: Vec<String> = credentials
        .iter()
        .map(|credential| credential.credential_id.clone())
        .collect();
    for member in db
        .list_mesh_observations(user_id, Some(network_name))
        .await?
    {
        if member.credential {
            continue;
        }
        let device_id = Uuid::parse_str(&member.device_id)?;
        let Some(view) = manager
            .local_configs(user_id, device_id)
            .await
            .ok()
            .flatten()
            .filter(|view| view.online && !view.stale)
        else {
            continue;
        };
        let Some(entry) = instance_entry(&view, &member.instance_id) else {
            continue;
        };
        let config = entry.config.as_ref().expect("ready entries carry a config");
        let placed = db
            .mesh_credential_delivery(user_id, &member.device_id, &member.instance_id)
            .await?;
        let mut next: Vec<_> = config
            .managed_credentials
            .iter()
            .filter(|credential| {
                !placed.contains(&credential.credential_id)
                    && !ours.contains(&credential.credential_id)
            })
            .cloned()
            .collect();
        next.extend(credentials.iter().cloned());
        if next != config.managed_credentials {
            let mut desired = config.clone();
            desired.managed_credentials = next;
            if let Err(error) = manager
                .patch_local_config(
                    user_id,
                    device_id,
                    PatchPersistedConfigRequest {
                        inst_id: entry.inst_id,
                        expected_revision: entry.revision.clone(),
                        config: Some(desired),
                        field_mask: vec!["managed_credentials".to_owned()],
                        toml: None,
                    },
                )
                .await
            {
                tracing::warn!(user_id, %device_id, %error, "temporary-node credentials were not placed");
                continue;
            }
        }
        if placed != ours {
            db.put_mesh_credential_delivery(
                user_id,
                &member.device_id,
                &member.instance_id,
                network_id,
                &ours,
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_copies_drop_the_default_wireguard_listener_once() {
        let listeners = |urls: &[&str]| urls.iter().map(|url| url.to_string()).collect::<Vec<_>>();
        let official = NetworkConfig {
            listener_urls: listeners(&[
                "tcp://0.0.0.0:11010",
                "udp://0.0.0.0:11010",
                OFFICIAL_DEFAULT_WG_LISTENER,
                "wg://0.0.0.0:22022",
            ]),
            ..Default::default()
        };
        let taken = official_defaults(&official).unwrap();
        assert_eq!(taken.enable_private_mode, Some(true));
        assert_eq!(
            taken.listener_urls,
            listeners(&[
                "tcp://0.0.0.0:11010",
                "udp://0.0.0.0:11010",
                "wg://0.0.0.0:22022"
            ])
        );
        // Once taken over, a WireGuard listener the user adds again stays.
        let readded = NetworkConfig {
            listener_urls: official.listener_urls.clone(),
            ..taken
        };
        assert!(official_defaults(&readded).is_none());
    }

    fn config(name: &str, secret: Option<&str>) -> NetworkConfig {
        NetworkConfig {
            network_name: Some(name.to_owned()),
            network_secret: secret.map(str::to_owned),
            hostname: Some("node".to_owned()),
            peer_urls: vec!["tcp://1.2.3.4:11010".to_owned()],
            ..Default::default()
        }
    }

    #[test]
    fn patches_only_differing_core_fields_and_keeps_node_keys() {
        let mut current = config("office", Some("old"));
        current.secure_mode = Some(SecureModeConfig {
            enabled: false,
            local_private_key: Some("node-key".to_owned()),
            local_public_key: Some("node-pub".to_owned()),
        });
        let change = CoreChange {
            network_secret: Some("new".to_owned()),
            secure_mode: Some(true),
            ..Default::default()
        };
        let target = change.apply_to(&CoreSettings::of(&current));
        let (desired, mask) = core_patch(&current, &target, Some(42), true).unwrap();
        assert_eq!(mask, ["network_secret", "secure_mode", "network_sync_at"]);
        assert_eq!(desired.network_secret.as_deref(), Some("new"));
        let secure = desired.secure_mode.unwrap();
        assert!(secure.enabled);
        assert_eq!(secure.local_private_key.as_deref(), Some("node-key"));
        assert_eq!(desired.hostname.as_deref(), Some("node"));
        assert_eq!(desired.peer_urls, current.peer_urls);
        assert_eq!(desired.network_sync_at, Some(42));

        assert!(core_patch(&current, &CoreSettings::of(&current), Some(43), true).is_none());
        let (_, mask) = core_patch(&current, &target, None, true).unwrap();
        assert!(!mask.contains(&"network_sync_at".to_owned()));
    }

    #[test]
    fn display_names_reach_only_nodes_that_keep_them() {
        let current = config("office", Some("secret"));
        let change = CoreChange {
            display_name: Some("Head office".into()),
            ..Default::default()
        };
        let target = change.apply_to(&CoreSettings::of(&current));
        let (desired, mask) = core_patch(&current, &target, None, true).unwrap();
        assert_eq!(mask, ["instance_name"]);
        assert_eq!(desired.instance_name.as_deref(), Some("Head office"));
        assert_eq!(desired.network_name.as_deref(), Some("office"));
        // Official cores always name the instance after the network.
        assert!(core_patch(&current, &target, None, false).is_none());
        let reset = CoreChange {
            display_name: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(reset.apply_to(&target).display_name, None);
    }

    #[test]
    fn credential_members_are_recognized() {
        let mut temporary = config("office", None);
        temporary.secure_mode = Some(SecureModeConfig {
            enabled: true,
            local_private_key: Some("credential".to_owned()),
            local_public_key: None,
        });
        assert!(is_credential_member(&temporary));
        let mut passwordless = temporary.clone();
        passwordless.network_secret = Some(String::new());
        assert!(!is_credential_member(&passwordless));
        assert!(!is_credential_member(&config("office", Some("secret"))));
    }

    #[test]
    fn later_changes_win_field_by_field() {
        let earlier = CoreChange {
            network_name: Some("a".into()),
            network_secret: Some("x".into()),
            ..Default::default()
        };
        let later = CoreChange {
            network_secret: Some("y".into()),
            ..Default::default()
        };
        let combined = earlier.then(&later);
        assert_eq!(combined.network_name.as_deref(), Some("a"));
        assert_eq!(combined.network_secret.as_deref(), Some("y"));
        let reset = CoreChange {
            encryption_algorithm: Some(String::new()),
            ..Default::default()
        };
        let settings = CoreSettings {
            encryption_algorithm: Some("chacha20".into()),
            ..Default::default()
        };
        assert_eq!(reset.apply_to(&settings).encryption_algorithm, None);
    }

    #[test]
    fn replays_overwrite_node_edits_and_older_consoles_but_not_newer_ones() {
        // Never stamped, or edited on the node: edits carry no stamp.
        assert!(replay_applies(None, 300));
        assert!(replay_applies(Some(100), 300));
        // Our own write landed already.
        assert!(replay_applies(Some(300), 300));
        // Another console already wrote a newer change.
        assert!(!replay_applies(Some(400), 300));
    }

    #[test]
    fn stamps_are_utc_and_sort_after_everything_seen() {
        let now = now_ms();
        let stamp = next_stamp([]);
        assert!(stamp >= now);
        let future = now + 3_600_000;
        assert_eq!(next_stamp([future]), future + 1);
    }

    #[test]
    fn subnet_changes_keep_host_parts_of_static_members_only() {
        let mut current = config("office", Some("s1"));
        current.dhcp = Some(false);
        current.virtual_ipv4 = Some("10.126.126.5".into());
        current.network_length = Some(24);
        let change = CoreChange {
            virtual_subnet: Some("10.20.30.7/24".into()),
            ..Default::default()
        };
        let target = change.apply_to(&CoreSettings::of(&current));
        assert_eq!(target.virtual_ipv4.as_deref(), Some("10.20.30.5/24"));
        let (desired, mask) = core_patch(&current, &target, Some(42), true).unwrap();
        assert_eq!(desired.virtual_ipv4.as_deref(), Some("10.20.30.5"));
        assert_eq!(desired.network_length, Some(24));
        assert_eq!(mask, ["virtual_ipv4", "network_sync_at"]);

        // Any prefix: the host bits under the new one are kept.
        let wide = CoreChange {
            virtual_subnet: Some("172.16.0.0/16".into()),
            ..Default::default()
        };
        let wide = wide.apply_to(&CoreSettings::of(&current));
        let (desired, _) = core_patch(&current, &wide, None, true).unwrap();
        assert_eq!(desired.virtual_ipv4.as_deref(), Some("172.16.126.5"));
        assert_eq!(desired.network_length, Some(16));

        // A replay may carry another member's address: only its subnet counts.
        let mut peer = CoreSettings::of(&current);
        peer.virtual_ipv4 = Some("10.20.30.9/24".into());
        let (desired, _) = core_patch(&current, &peer, None, true).unwrap();
        assert_eq!(desired.virtual_ipv4.as_deref(), Some("10.20.30.5"));

        // DHCP members follow the others on their own.
        let mut dhcp = config("office", Some("s1"));
        dhcp.dhcp = Some(true);
        let target = change.apply_to(&CoreSettings::of(&dhcp));
        assert!(core_patch(&dhcp, &target, None, true).is_none());

        // The network's own settings hold the subnet.
        let record = change.apply_to(&CoreSettings::default());
        assert_eq!(record.virtual_ipv4.as_deref(), Some("10.20.30.0/24"));

        // A host part that would be the new subnet's network address stays put.
        let narrow = CoreChange {
            virtual_subnet: Some("10.20.30.0/30".into()),
            ..Default::default()
        };
        current.virtual_ipv4 = Some("10.126.126.4".into());
        let target = narrow.apply_to(&CoreSettings::of(&current));
        assert!(core_patch(&current, &target, None, true).is_none());

        // A DHCP member's configured subnet is recorded apart from addresses.
        let mut ranged = config("office", Some("s1"));
        ranged.dhcp = Some(true);
        ranged.dhcp_ipv4_cidr = Some("10.55.0.9/24".into());
        let settings = CoreSettings::of(&ranged);
        assert_eq!(settings.virtual_ipv4, None);
        assert_eq!(settings.dhcp_ipv4_cidr.as_deref(), Some("10.55.0.0/24"));

        let missing_prefix = CoreChange {
            virtual_subnet: Some("10.20.30.0".into()),
            ..Default::default()
        };
        assert!(missing_prefix.validate().is_err());
    }

    #[test]
    fn empty_names_and_blank_secrets_are_rejected() {
        let name = CoreChange {
            network_name: Some("  ".into()),
            ..Default::default()
        };
        assert!(name.validate().is_err());
        let blank = CoreChange {
            network_secret: Some("   ".into()),
            ..Default::default()
        };
        assert!(blank.validate().is_err());
        let passwordless = CoreChange {
            network_secret: Some(String::new()),
            ..Default::default()
        };
        assert!(passwordless.validate().is_ok());
    }
}
