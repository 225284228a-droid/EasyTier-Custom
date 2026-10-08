//! Permission inspection of original bytes without reading environment values.

use std::{
    borrow::Cow,
    convert::Infallible,
    path::{Path, PathBuf},
};

use easytier_core::management::{ConfigFileControl, ConfigFilePermission};

/// Match the native loader's shell syntax, including dollar escaping, while
/// supplying a fixed synthetic context. Secrets never enter this read path.
fn requires_environment_expansion(contents: &str) -> bool {
    matches!(
        shellexpand::env_with_context(contents, |_| Ok::<_, Infallible>(Some(""))),
        Ok(Cow::Owned(_))
    )
}

pub(super) async fn inspect_raw(
    path: &Path,
    contents: &[u8],
    config_dir: &Path,
    disable_env_parsing: bool,
) -> anyhow::Result<ConfigFileControl> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let text = std::str::from_utf8(contents).ok();
    let table = text.and_then(|text| toml::from_str::<toml::Table>(text).ok());
    let identity = table
        .as_ref()
        .and_then(|table| table.get("instance_id"))
        .and_then(toml::Value::as_str)
        .and_then(|id| id.parse::<uuid::Uuid>().ok());
    let managed = path.parent() == Some(config_dir)
        && path.extension() == Some(std::ffi::OsStr::new("toml"))
        && identity.is_some_and(|id| {
            path.file_stem().and_then(|stem| stem.to_str()) == Some(id.to_string().as_str())
        });
    let read_only = metadata.as_ref().is_some_and(|metadata| {
        metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.permissions().readonly()
    }) || (!disable_env_parsing
        && text.is_some_and(requires_environment_expansion));
    let permission = ConfigFilePermission::from(if read_only {
        ConfigFilePermission::READ_ONLY | ConfigFilePermission::NO_DELETE
    } else if !managed {
        ConfigFilePermission::NO_DELETE
    } else {
        0
    });
    Ok(ConfigFileControl::new(Some(path.to_owned()), permission))
}

pub(super) async fn list_configs(directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut reader = match tokio::fs::read_dir(directory).await {
        Ok(reader) => reader,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    while let Some(entry) = reader.next_entry().await? {
        let path = entry.path();
        if path.extension() == Some(std::ffi::OsStr::new("toml")) {
            // Symlinks remain metadata-only protected catalog entries.
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_syntax_is_detected_without_environment_lookup() {
        for input in [
            "$SECRET",
            "${SECRET}",
            "${SECRET:-fallback}",
            "$${SECRET}",
            "$$",
        ] {
            assert!(requires_environment_expansion(input));
        }
        assert!(!requires_environment_expansion("ordinary TOML text"));
    }

    #[tokio::test]
    async fn raw_policy_protects_env_and_filename_identity_without_expansion() {
        let directory = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4();
        let path = directory.path().join(format!("{id}.toml"));
        let raw = format!(
            "instance_id='{id}'\n[network_identity]\nnetwork_secret='${{SOME_PRIVATE_VALUE:-fallback}}'\n"
        );
        std::fs::write(&path, &raw).unwrap();
        let control = inspect_raw(&path, raw.as_bytes(), directory.path(), false)
            .await
            .unwrap();
        assert!(control.is_read_only());
        assert!(!control.is_deletable());
        let control = inspect_raw(&path, raw.as_bytes(), directory.path(), true)
            .await
            .unwrap();
        assert!(!control.is_read_only());
        assert!(control.is_deletable());
        let foreign = directory.path().join("some-name.toml");
        let control = inspect_raw(&foreign, raw.as_bytes(), directory.path(), true)
            .await
            .unwrap();
        assert!(!control.is_read_only());
        assert!(!control.is_deletable());
    }
}
