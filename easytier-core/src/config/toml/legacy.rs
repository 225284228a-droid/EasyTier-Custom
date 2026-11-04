//! Loading migration for TOML files written before `p2p_disguise_mode`.
//!
//! Kept out of `toml.rs` so the upstream file only calls the migration.

use anyhow::Context;

use crate::config::P2pDisguiseMode;

pub(crate) const LEGACY_P2P_DISGUISE_FLAGS: [&str; 3] = [
    "prefer_wss_http3_for_p2p",
    "disable_wss_http3_for_p2p",
    "only_use_wss_http3_for_hole_punching",
];

/// Files written before `p2p_disguise_mode` existed state the policy with three
/// booleans in `[flags]`. Rewrite them as the equivalent mode (a mode that is
/// already set wins); the next save of the file stores only the mode.
/// Returns `None` when the document has nothing to migrate.
pub(crate) fn migrate_legacy_p2p_disguise_flags(
    config_str: &str,
) -> anyhow::Result<Option<String>> {
    if !LEGACY_P2P_DISGUISE_FLAGS
        .iter()
        .any(|key| config_str.contains(key))
    {
        return Ok(None);
    }
    // Syntax errors are reported against the original document.
    let Ok(mut document) = toml::from_str::<toml::Table>(config_str) else {
        return Ok(None);
    };
    let Some(flags) = document
        .get_mut("flags")
        .and_then(toml::Value::as_table_mut)
    else {
        return Ok(None);
    };
    let mut legacy = [None; 3];
    for (value, key) in legacy.iter_mut().zip(LEGACY_P2P_DISGUISE_FLAGS) {
        if let Some(raw) = flags.remove(key) {
            *value = Some(
                raw.as_bool()
                    .with_context(|| format!("[flags] {key} must be a boolean"))?,
            );
        }
    }
    if legacy.iter().all(Option::is_none) {
        return Ok(None);
    }
    let mode_set = flags
        .get("p2p_disguise_mode")
        .and_then(toml::Value::as_str)
        .is_some_and(|mode| !mode.trim().is_empty());
    if !mode_set {
        let mode = P2pDisguiseMode::from_legacy_flags(legacy[0], legacy[1], legacy[2])
            .context("invalid legacy P2P disguise flags")?;
        flags.insert(
            "p2p_disguise_mode".to_owned(),
            toml::Value::String(mode.as_str().to_owned()),
        );
    }
    Ok(Some(toml::to_string(&document)?))
}
