// Shared config directory helpers, kept out of `lib.rs` to limit upstream
// merge conflicts. Logic is unchanged; see git history for the original block.

use super::*;

pub(crate) fn shared_config_dir(
    app: &AppHandle,
    configured: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    let path = match configured.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => {
            let path = std::path::PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .map_err(|error| error.to_string())?
                    .join(path)
            }
        }
        None => {
            if cfg!(target_os = "android") {
                app.path()
                    .app_data_dir()
                    .map_err(|error| error.to_string())?
                    .join("network-configs")
            } else {
                app.path()
                    .app_config_dir()
                    .map_err(|error| error.to_string())?
                    .join("config.d")
            }
        }
    };
    std::fs::create_dir_all(&path)
        .with_context(|| format!("Failed to create config dir {}", path.display()))
        .map_err(|error| format!("{error:#}"))?;
    std::fs::canonicalize(&path)
        .with_context(|| format!("Failed to resolve config dir {}", path.display()))
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub(crate) fn resolve_shared_config_dir(
    app: AppHandle,
    config_dir: Option<String>,
) -> Result<String, String> {
    Ok(shared_config_dir(&app, config_dir.as_deref())?
        .to_string_lossy()
        .to_string())
}

pub(crate) fn gui_machine_id(app: &AppHandle) -> Result<Uuid, String> {
    let state_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    resolve_machine_id(&MachineIdOptions {
        explicit_machine_id: None,
        state_dir: Some(state_dir),
    })
    .map_err(|error| format!("{error:#}"))
}

pub(crate) async fn restore_managed_configs(
    manager: &NativeInstanceManager,
    state_store: &InstanceStateStore,
    config_dir: &std::path::Path,
) -> anyhow::Result<()> {
    // Share the CLI startup policy (see `easytier::instance::config_dir_startup`):
    // invalid entries and duplicate instance UUIDs are isolated with a
    // warning so a single broken file cannot keep the whole local backend -
    // and with it the configuration editor that could repair the file - from
    // starting. Entries that fail to start are skipped as well.
    let mut startup =
        easytier::instance::config_dir_startup::load_config_dir_entries(config_dir, false).await?;

    for file in startup.files.drain(..) {
        let result = (|| -> anyhow::Result<()> {
            manager.register_managed_config(file.config.get_id(), file.control.clone())?;
            if state_store.is_enabled(&file.config.get_id()) {
                manager.run_network_instance(file.config, file.control)?;
            }
            if let Some(contents) = file.original_contents.as_deref() {
                manager
                    .local_config_catalog()
                    .acknowledge_file(&file.path, contents);
            }
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, config_file = ?file.path, "Skipping config directory entry that could not be started");
        }
    }

    // Match the service core's discovered defaults and deleted-file cleanup.
    startup.reconcile_state(state_store);
    Ok(())
}
