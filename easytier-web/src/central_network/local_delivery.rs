//! Central intent is a publication into node-owned files, never a periodic
//! replacement of their catalog. Revisions also protect manual local edits.
use std::collections::BTreeMap;

use easytier::{
    common::config::{NetworkConfigExt as _, TomlConfigLoader},
    proto::api::manage::{NetworkConfig, PatchPersistedConfigRequest, PersistedConfigEntry},
};
use easytier_core::config::api::network_config_from_toml;
use uuid::Uuid;

use crate::{
    client_manager::{
        ClientManager,
        session::local_configs::{LocalConfigError, LocalConfigView},
    },
    db::{CentralLocalDelivery, Db, UserIdInDb},
};

const CONFLICT: &str =
    "central configuration delivery conflicted with a local change; local files were preserved";
const UNKNOWN: &str = "central configuration write outcome is unknown; local files were preserved";

pub(super) fn canonical(config: &NetworkConfig) -> anyhow::Result<NetworkConfig> {
    let mut canonical = network_config_from_toml(&config.gen_config()?);
    if let Some(intent) = config.secure_mode.as_ref()
        && intent.local_private_key.is_none()
        && let Some(normalized) = canonical.secure_mode.as_mut()
    {
        // A regular secure member's local identity belongs to the node. Its
        // generated key must never turn stable intent into a new publication.
        normalized.local_private_key = None;
        normalized.local_public_key = intent.local_public_key.clone();
    }
    Ok(canonical)
}

fn preserve_node_identity(desired: &mut NetworkConfig, observed: &NetworkConfig) {
    if let Some(intent) = desired.secure_mode.as_mut()
        && intent.enabled
        && intent.local_private_key.is_none()
        && let Some(identity) = observed
            .secure_mode
            .as_ref()
            .filter(|identity| identity.enabled)
    {
        intent.local_private_key = identity.local_private_key.clone();
        intent.local_public_key = identity.local_public_key.clone();
    }
}

fn entry(
    mirror: &LocalConfigView,
    instance_id: Uuid,
) -> anyhow::Result<Option<PersistedConfigEntry>> {
    let mut entries = mirror
        .snapshot
        .entries
        .iter()
        .filter(|entry| entry.inst_id.map(Uuid::from) == Some(instance_id));
    let first = entries.next().cloned();
    anyhow::ensure!(entries.next().is_none(), "{CONFLICT}");
    if let Some(entry) = &first {
        anyhow::ensure!(entry.status == "ready", "{CONFLICT}");
    }
    Ok(first)
}

/// Never adopt the latest node revision to retry a conflicting publication.
/// Revisions identify file contents and survive node restarts, so any other
/// revision is a local or foreign edit (or an unknown outcome of our write).
fn check_baseline(
    row: &CentralLocalDelivery,
    observed: Option<&PersistedConfigEntry>,
) -> anyhow::Result<()> {
    match (row.revision.as_deref(), observed) {
        (Some(revision), Some(entry)) if entry.revision == revision => Ok(()),
        (None, None) => Ok(()),
        _ if row.pending_operation => anyhow::bail!("{UNKNOWN}"),
        _ => anyhow::bail!("{CONFLICT}"),
    }
}

fn creation_baseline(instance_id: Uuid) -> anyhow::Result<NetworkConfig> {
    Ok(network_config_from_toml(&TomlConfigLoader::new_from_str(
        &format!(
            "instance_id = '{instance_id}'\n[network_identity]\nnetwork_name = ''\nnetwork_secret = ''\n"
        ),
    )?))
}

fn field_mask(current: &NetworkConfig, desired: &NetworkConfig) -> anyhow::Result<Vec<String>> {
    let current = serde_json::to_value(current)?;
    let desired = serde_json::to_value(desired)?;
    let current = current.as_object().unwrap();
    let desired = desired.as_object().unwrap();
    let fields = current
        .keys()
        .chain(desired.keys())
        .filter(|field| field.as_str() != "instance_id")
        .collect::<std::collections::BTreeSet<_>>();
    Ok(fields
        .into_iter()
        .filter(|field| current.get(*field) != desired.get(*field))
        .cloned()
        .collect())
}

async fn observe(
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
) -> anyhow::Result<LocalConfigView> {
    // A cached offline mirror is never a mutation baseline.
    let session = manager
        .get_session_by_machine_id(user_id, &device_id)
        .ok_or_else(|| {
            anyhow::anyhow!("device is offline; central configuration delivery is pending")
        })?;
    anyhow::ensure!(
        session.has_local_revision_capability().await,
        "node does not support persisted revision management; central configuration delivery is pending"
    );
    session.observe_local_configs().await
}

async fn save_revision(
    db: &Db,
    user_id: UserIdInDb,
    device_id: Uuid,
    row: &mut CentralLocalDelivery,
    observed: &PersistedConfigEntry,
) -> anyhow::Result<()> {
    row.revision = Some(observed.revision.clone());
    row.baseline = None;
    row.pending_operation = false;
    row.error = None;
    store(db, user_id, device_id, row).await?;
    Ok(())
}

async fn store(
    db: &Db,
    user_id: UserIdInDb,
    device_id: Uuid,
    row: &CentralLocalDelivery,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        db.update_central_local_delivery(user_id, device_id, row)
            .await?,
        "central desired publication changed; delivery will be retried"
    );
    Ok(())
}

async fn deliver(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
    row: &mut CentralLocalDelivery,
) -> anyhow::Result<()> {
    let instance_id = Uuid::parse_str(&row.instance_id)?;
    let mirror = observe(manager, user_id, device_id).await?;
    let mut observed = entry(&mirror, instance_id)?;
    if row.desired_config.is_none() && observed.is_none() {
        // This covers confirmed deletion and an undelivered tombstone. A fresh
        // node read fences a timed-out initial create before retiring it.
        db.remove_central_local_delivery(user_id, device_id, instance_id)
            .await?;
        return Ok(());
    }
    check_baseline(row, observed.as_ref())?;
    if row.desired_config.is_none() {
        let revision = row
            .revision
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{CONFLICT}"))?;
        row.pending_operation = true;
        store(db, user_id, device_id, row).await?;
        let mirror = manager
            .mutate_local_config_lifecycle(user_id, device_id, instance_id, revision, None)
            .await?;
        anyhow::ensure!(entry(&mirror, instance_id)?.is_none(), "{CONFLICT}");
        db.remove_central_local_delivery(user_id, device_id, instance_id)
            .await?;
        return Ok(());
    }
    let mut desired: NetworkConfig = serde_json::from_str(row.desired_config.as_ref().unwrap())?;
    let baseline = observed
        .as_ref()
        .and_then(|entry| entry.config.clone())
        .map(Ok)
        .unwrap_or_else(|| creation_baseline(instance_id))?;
    preserve_node_identity(&mut desired, &baseline);
    let mask = field_mask(&baseline, &desired)?;
    if !mask.is_empty() {
        row.pending_operation = true;
        store(db, user_id, device_id, row).await?;
        let mirror = manager
            .patch_local_config(
                user_id,
                device_id,
                PatchPersistedConfigRequest {
                    inst_id: Some(instance_id.into()),
                    expected_revision: row.revision.clone().unwrap_or_default(),
                    config: Some(desired),
                    field_mask: mask,
                    toml: None,
                },
            )
            .await?;
        observed = entry(&mirror, instance_id)?;
        save_revision(
            db,
            user_id,
            device_id,
            row,
            observed
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("{UNKNOWN}"))?,
        )
        .await?;
    }
    let mut observed = observed.ok_or_else(|| anyhow::anyhow!("{UNKNOWN}"))?;
    // Creating a persisted config deliberately starts disabled. Starting it
    // uses its fresh CAS revision, so an intervening local edit stays intact.
    if !observed.enabled || !observed.running {
        row.pending_operation = true;
        store(db, user_id, device_id, row).await?;
        let mirror = manager
            .mutate_local_config_lifecycle(
                user_id,
                device_id,
                instance_id,
                observed.revision.clone(),
                Some(true),
            )
            .await?;
        let enabled = entry(&mirror, instance_id)?.ok_or_else(|| anyhow::anyhow!("{UNKNOWN}"))?;
        // Lifecycle responses do not carry an entry revision. Enabling is
        // allowed to change runtime state, while a concurrent file edit or
        // permission change must never become central's acknowledged baseline.
        anyhow::ensure!(
            observed.persisted_raw_hash == enabled.persisted_raw_hash
                && observed.config_permission == enabled.config_permission
                && enabled.enabled
                && enabled.running,
            "{CONFLICT}"
        );
        observed = enabled;
        save_revision(db, user_id, device_id, row, &observed).await?;
    }
    anyhow::ensure!(
        observed.running,
        "central configuration could not be applied"
    );
    row.delivered_config = row.desired_config.clone();
    row.error = None;
    row.pending_operation = false;
    store(db, user_id, device_id, row).await?;
    Ok(())
}

pub(crate) async fn publish(
    db: &Db,
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
    configs: Vec<crate::webhook::ManagedNetworkConfig>,
) -> anyhow::Result<()> {
    let configs = configs
        .into_iter()
        .map(|config| {
            let instance_id = Uuid::parse_str(&config.instance_id)?;
            let config = serde_json::from_value(config.network_config)?;
            Ok((instance_id, canonical(&config)?))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let rows = db
        .project_central_local_configs(user_id, device_id, &configs)
        .await?;
    for mut row in rows {
        // An acknowledged publication is complete. Never restore its runtime
        // or file merely because a periodic central reconcile runs again.
        if row.desired_config.is_some()
            && row.desired_config == row.delivered_config
            && !row.pending_operation
        {
            if row.error.take().is_some() {
                store(db, user_id, device_id, &row).await?;
            }
            continue;
        }
        if let Err(error) = deliver(db, manager, user_id, device_id, &mut row).await {
            // Typed revision conflicts must remain conflicts, even though the
            // attempted write is journaled before the RPC for crash recovery.
            if error
                .downcast_ref::<LocalConfigError>()
                .is_some_and(|error| {
                    matches!(
                        error,
                        LocalConfigError::Conflict(_)
                            | LocalConfigError::Protected
                            | LocalConfigError::Invalid(_)
                            | LocalConfigError::Unsupported
                    )
                })
            {
                row.pending_operation = false;
            }
            row.error = Some(error.to_string());
            store(db, user_id, device_id, &row).await?;
            tracing::warn!(user_id, %device_id, instance_id = row.instance_id, %error, "central local configuration delivery is pending");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_baselines_never_adopt_local_changes_or_foreign_identities() {
        let mut row = CentralLocalDelivery {
            instance_id: Uuid::new_v4().to_string(),
            desired_config: Some("{}".into()),
            delivered_config: None,
            revision: None,
            baseline: None,
            pending_operation: false,
            error: None,
        };
        let entry = PersistedConfigEntry {
            revision: "node-v2".into(),
            ..Default::default()
        };
        assert!(check_baseline(&row, None).is_ok());
        assert!(
            check_baseline(&row, Some(&entry))
                .unwrap_err()
                .to_string()
                .contains("conflicted")
        );
        row.revision = Some("node-v1".into());
        assert!(check_baseline(&row, None).is_err());
        assert!(check_baseline(&row, Some(&entry)).is_err());
        row.pending_operation = true;
        assert!(
            check_baseline(&row, Some(&entry))
                .unwrap_err()
                .to_string()
                .contains("unknown")
        );
        row.revision = Some("node-v2".into());
        assert!(check_baseline(&row, Some(&entry)).is_ok());
    }

    #[test]
    fn masks_clear_removed_values_and_do_not_select_unsupported_default_extensions() {
        let id = Uuid::new_v4();
        let mut desired = creation_baseline(id).unwrap();
        desired.network_name = Some("central".into());
        desired.network_secret = Some("secret".into());
        let mut current = desired.clone();
        current.sni = Some("old.example".into());
        let mask = field_mask(&current, &desired).unwrap();
        assert_eq!(mask, vec!["sni"]);
        assert!(
            !field_mask(&creation_baseline(id).unwrap(), &desired)
                .unwrap()
                .iter()
                .any(|field| field == "p2p_prefer_protocol")
        );
    }

    #[test]
    fn secure_intent_is_stable_and_updates_preserve_the_nodes_generated_identity() {
        let config = NetworkConfig {
            instance_id: Some(Uuid::new_v4().to_string()),
            network_name: Some("secure-central".into()),
            network_secret: Some("secret".into()),
            networking_method: Some(2),
            secure_mode: Some(easytier::proto::common::SecureModeConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(canonical(&config).unwrap(), canonical(&config).unwrap());
        let observed = network_config_from_toml(&config.gen_config().unwrap());
        let identity = observed.secure_mode.clone().unwrap();
        assert!(identity.local_private_key.is_some());
        let mut update = canonical(&config).unwrap();
        preserve_node_identity(&mut update, &observed);
        assert_eq!(update.secure_mode, Some(identity));
        assert!(
            !field_mask(&observed, &update)
                .unwrap()
                .contains(&"secure_mode".into())
        );
    }
}
