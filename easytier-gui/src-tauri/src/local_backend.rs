//! Tauri commands that drive the service/normal backend lifecycle.
//!
//! Split out of `lib.rs` so the upstream file only wires the commands.

use super::*;

#[tauri::command]
pub(crate) async fn set_service_status(
    enable: bool,
    rpc_portal: Option<String>,
) -> Result<(), String> {
    let _lifecycle = BACKEND_LIFECYCLE.lock().await;
    if enable {
        stop_local_backend_inner().await?;
    }
    #[cfg(not(target_os = "android"))]
    {
        // `ensure_rpc_port_free` binds a socket and `set_status` drives the
        // SCM (service start/stop can block for seconds) - keep both off the
        // async threads.
        tokio::task::spawn_blocking(move || {
            if enable {
                let portal = rpc_portal
                    .as_deref()
                    .ok_or("RPC portal is required to start the service")?;
                service_conflicts::ensure_rpc_port_free(portal)
                    .map_err(|error| format!("{error:#}"))?;
            }
            service::set_status(enable).map_err(|e| format!("{:#}", e))
        })
        .await
        .map_err(|e| e.to_string())??;
    }
    #[cfg(target_os = "android")]
    let _ = rpc_portal;
    Ok(())
}

#[tauri::command]
pub(crate) async fn retire_conflicting_services(
    app: AppHandle,
    config_dir: String,
    rpc_portal: Option<String>,
    allow_uninstall: bool,
) -> Result<Vec<service_conflicts::RetireOutcome>, String> {
    let config_dir = shared_config_dir(&app, Some(&config_dir))?;
    tokio::task::spawn_blocking(move || {
        service_conflicts::retire_conflicting_services(
            &config_dir,
            rpc_portal.as_deref(),
            allow_uninstall,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| format!("{error:#}"))
}

pub(crate) async fn stop_local_backend_inner() -> Result<(), String> {
    let manager = INSTANCE_MANAGER.read().await.clone();
    if let Some(manager) = &manager {
        manager.local_config_catalog().retire();
    }
    let web_clients = std::mem::take(&mut *WEB_CLIENT.write().await);
    for client in web_clients {
        client.shutdown().await;
    }
    let client = CLIENT_MANAGER.write().await.take();
    if let Some(client) = client {
        client.rpc_manager.stop().await;
    }
    let server = RPC_SERVER.lock().await.take();
    if let Some(mut server) = server {
        server._server.shutdown().await;
    }
    if let Some(manager) = manager {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let lock = manager.mutation_lock();
            let _mutation = lock.lock().await;
            manager.retain_network_instances(&[]).await
        })
        .await
        .map_err(|_| "Timed out waiting for local network instances to stop".to_string())?
        .map_err(|error| error.to_string())?;
    }
    *INSTANCE_MANAGER.write().await = None;
    *INSTANCE_STATE_STORE.write().await = None;
    *INSTANCE_CONFIG_DIR.write().await = None;
    manager::PREPARED_PROFILES.clear();
    Ok(())
}

/// Release the embedded core before a service claims its ports and TUN devices.
#[tauri::command]
pub(crate) async fn stop_local_backend() -> Result<(), String> {
    let _lifecycle = BACKEND_LIFECYCLE.lock().await;
    stop_local_backend_inner().await
}

#[tauri::command]
pub(crate) async fn get_service_status() -> Result<&'static str, String> {
    #[cfg(not(target_os = "android"))]
    {
        use easytier::service_manager::ServiceStatus;
        let status = tokio::task::spawn_blocking(service::status)
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{:#}", e))?;
        match status {
            ServiceStatus::NotInstalled => Ok("NotInstalled"),
            ServiceStatus::Stopped(_) => Ok("Stopped"),
            ServiceStatus::Running => Ok("Running"),
        }
    }
    #[cfg(target_os = "android")]
    {
        Ok("NotInstalled")
    }
}
