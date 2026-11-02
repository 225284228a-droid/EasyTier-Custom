//! Core settings are the few values every peer of a network must share to
//! stay connected: the network name, the secret, the secure mode switch and
//! encryption. A console writes a change of them to the members it can reach
//! now. A member that was offline receives the missed change once, the next
//! time it connects, and never again; a change made on the node is never
//! reverted. Nothing else in a member's configuration is touched.
//!
//! Several consoles can manage one node. Each change carries a UTC timestamp
//! stored on the node (`network_sync_at`), so a late replay never overwrites
//! a newer change another console already made.

use std::time::{SystemTime, UNIX_EPOCH};

use easytier::proto::{
    api::manage::{
        ManagedCredentialConfig, NetworkConfig, PatchPersistedConfigRequest, PersistedConfigEntry,
    },
    common::SecureModeConfig,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    client_manager::{
        ClientManager,
        session::local_configs::{LocalConfigError, LocalConfigView},
    },
    db::{Db, MeshCoreReplay, MeshObservation, UserIdInDb},
};

pub(crate) const SYNC_CAPABILITY: &str = "config:network-sync-v1";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreSettings {
    pub network_name: String,
    /// `None` is a credential identity; `Some("")` a passwordless network.
    pub network_secret: Option<String>,
    pub secure_mode: bool,
    pub disable_encryption: bool,
    pub encryption_algorithm: Option<String>,
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
        }
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
        Ok(())
    }
}

/// A temporary member authenticates with a credential instead of the secret.
fn is_credential_member(config: &NetworkConfig) -> bool {
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
    if mask.is_empty() {
        return None;
    }
    if let Some(stamp) = stamp {
        desired.network_sync_at = Some(stamp);
        mask.push("network_sync_at".to_owned());
    }
    Some((desired, mask))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// UTC milliseconds that sort after every change already seen, even when
/// this console's clock runs behind the console that made it.
fn next_stamp(seen: impl IntoIterator<Item = u64>) -> u64 {
    let now = now_ms();
    seen.into_iter()
        .fold(now, |stamp, seen| stamp.max(seen + 1))
}

fn instance_entry(view: &LocalConfigView, instance_id: &str) -> Option<PersistedConfigEntry> {
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

fn observations_of(view: &LocalConfigView) -> anyhow::Result<Vec<MeshObservation>> {
    let mut observations = Vec::new();
    for entry in &view.snapshot.entries {
        let (Some(config), Some(instance_id)) = (&entry.config, entry.inst_id) else {
            continue;
        };
        if entry.status != "ready" {
            continue;
        }
        let core = CoreSettings::of(config);
        observations.push(MeshObservation {
            device_id: view.machine_id.to_string(),
            instance_id: Uuid::from(instance_id).to_string(),
            network_name: core.network_name.clone(),
            core: serde_json::to_string(&core)?,
            sync_at: config.network_sync_at.map(|stamp| stamp as i64),
            credential: is_credential_member(config),
        });
    }
    Ok(observations)
}

/// Record what each online revision-capable device holds now. Offline
/// devices keep their last observation.
pub(crate) async fn observe_user(db: &Db, manager: &ClientManager, user_id: UserIdInDb) {
    let devices = match db.list_user_device_ids(user_id).await {
        Ok(devices) => devices,
        Err(error) => {
            tracing::warn!(user_id, %error, "cannot list devices for core sync");
            return;
        }
    };
    for device_id in devices {
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

async fn write_core(
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
    view: &LocalConfigView,
    entry: &PersistedConfigEntry,
    target: &CoreSettings,
    stamp: u64,
) -> Result<bool, LocalConfigError> {
    let config = entry.config.as_ref().expect("ready entries carry a config");
    let stamp = view
        .capabilities
        .iter()
        .any(|capability| capability == SYNC_CAPABILITY)
        .then_some(stamp);
    let Some((desired, mask)) = core_patch(config, target, stamp) else {
        return Ok(false);
    };
    manager
        .patch_local_config(
            user_id,
            device_id,
            PatchPersistedConfigRequest {
                inst_id: entry.inst_id,
                expected_revision: entry.revision.clone(),
                config: Some(desired),
                field_mask: mask,
                toml: None,
            },
        )
        .await?;
    Ok(true)
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
    for member in members {
        let device_id = Uuid::parse_str(&member.device_id)?;
        let result = |status, message: Option<String>| MemberSyncResult {
            device_id: member.device_id.clone(),
            instance_id: member.instance_id.clone(),
            status,
            message,
        };
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
        let target = change.apply_to(&CoreSettings::of(config));
        results.push(
            match write_core(manager, user_id, device_id, &view, &entry, &target, stamp).await {
                Ok(true) => result(MemberSyncStatus::Saved, None),
                Ok(false) => result(MemberSyncStatus::Unchanged, None),
                Err(LocalConfigError::Conflict(_)) => result(MemberSyncStatus::Conflict, None),
                Err(error) => result(MemberSyncStatus::Failed, Some(error.to_string())),
            },
        );
    }
    observe_user(db, manager, user_id).await;
    Ok(results)
}

/// Whether a missed change may still be sent: the member was not changed
/// on the node since the console last saw it, and it holds nothing newer
/// than the change. Another console's write counts as newer only when its
/// stamp is.
fn replay_applies(current: &CoreSettings, current_stamp: u64, replay: &MeshCoreReplay) -> bool {
    let base_stamp = replay.base_sync_at.unwrap_or(0) as u64;
    let target_stamp = replay.target_sync_at as u64;
    let untouched = serde_json::from_str::<CoreSettings>(&replay.base)
        .is_ok_and(|base| base == *current)
        && current_stamp <= base_stamp;
    let rewritten_by_older_console = current_stamp > base_stamp && current_stamp < target_stamp;
    untouched || rewritten_by_older_console
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
        let Some(entry) = instance_entry(&view, &replay.instance_id) else {
            continue;
        };
        let config = entry.config.as_ref().expect("ready entries carry a config");
        let current = CoreSettings::of(config);
        let current_stamp = config.network_sync_at.unwrap_or(0);
        if !replay_applies(&current, current_stamp, &replay) {
            tracing::info!(user_id, %device_id, instance_id = replay.instance_id, "missed core change no longer applies");
            continue;
        }
        let Ok(change) = serde_json::from_str::<CoreChange>(&replay.change) else {
            continue;
        };
        let mut target = change.apply_to(&current);
        let mut stamp = replay.target_sync_at as u64;
        // Catch up to the newest settings this console has seen for the
        // network, even when another console made that change.
        if let Ok(peers) = db
            .list_mesh_observations(user_id, Some(&target.network_name))
            .await
            && let Some(newest) = peers
                .iter()
                .filter(|peer| !peer.credential && peer.instance_id != replay.instance_id)
                .filter(|peer| peer.sync_at.is_some_and(|seen| seen as u64 > stamp))
                .max_by_key(|peer| peer.sync_at)
            && let Ok(settings) = serde_json::from_str::<CoreSettings>(&newest.core)
        {
            target = settings;
            stamp = newest.sync_at.unwrap_or_default() as u64;
        }
        match write_core(manager, user_id, device_id, &view, &entry, &target, stamp).await {
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(user_id, %device_id, instance_id = replay.instance_id, %error, "missed core change was not applied")
            }
        }
    }
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

    fn config(name: &str, secret: Option<&str>) -> NetworkConfig {
        NetworkConfig {
            network_name: Some(name.to_owned()),
            network_secret: secret.map(str::to_owned),
            hostname: Some("node".to_owned()),
            peer_urls: vec!["tcp://1.2.3.4:11010".to_owned()],
            ..Default::default()
        }
    }

    fn replay(
        base: &CoreSettings,
        base_sync_at: Option<i64>,
        target_sync_at: i64,
    ) -> MeshCoreReplay {
        MeshCoreReplay {
            device_id: String::new(),
            instance_id: String::new(),
            base: serde_json::to_string(base).unwrap(),
            base_sync_at,
            change: "{}".to_owned(),
            target_sync_at,
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
        let (desired, mask) = core_patch(&current, &target, Some(42)).unwrap();
        assert_eq!(mask, ["network_secret", "secure_mode", "network_sync_at"]);
        assert_eq!(desired.network_secret.as_deref(), Some("new"));
        let secure = desired.secure_mode.unwrap();
        assert!(secure.enabled);
        assert_eq!(secure.local_private_key.as_deref(), Some("node-key"));
        assert_eq!(desired.hostname.as_deref(), Some("node"));
        assert_eq!(desired.peer_urls, current.peer_urls);
        assert_eq!(desired.network_sync_at, Some(42));

        assert!(core_patch(&current, &CoreSettings::of(&current), Some(43)).is_none());
        let (_, mask) = core_patch(&current, &target, None).unwrap();
        assert!(!mask.contains(&"network_sync_at".to_owned()));
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
    fn replay_applies_only_to_untouched_or_older_console_writes() {
        let base = CoreSettings::of(&config("office", Some("old")));
        let pending = replay(&base, Some(100), 300);
        // Untouched since the console saw it.
        assert!(replay_applies(&base, 100, &pending));
        // Edited on the node: the stamp stays but the values changed.
        let edited = CoreSettings::of(&config("office", Some("hand-edited")));
        assert!(!replay_applies(&edited, 100, &pending));
        // Another console wrote an older change after the console saw it.
        assert!(replay_applies(&edited, 200, &pending));
        // Another console already wrote a newer change.
        assert!(!replay_applies(&edited, 400, &pending));
        // A member never stamped before.
        let unstamped = replay(&base, None, 300);
        assert!(replay_applies(&base, 0, &unstamped));
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
