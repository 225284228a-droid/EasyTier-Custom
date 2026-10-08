// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod elevate;
mod service_conflicts;

use anyhow::Context;
#[cfg(target_os = "android")]
use easytier::instance::factory::subscribe_native_instance_event;
use easytier::proto::api::config::{
    ConfigPatchAction, ConfigRpc, ConfigRpcClientFactory, VpnPortalClientPatch,
};
use easytier::proto::api::instance::{
    GetVpnPortalInfoRequest, InstanceIdentifier, VpnPortalInfo, VpnPortalRpc,
    VpnPortalRpcClientFactory, instance_identifier,
};
use easytier::proto::api::manage::{
    CollectNetworkInfoResponse, ValidateConfigResponse, VpnPortalClientConfig, WebClientService,
    WebClientServiceClientFactory,
};
use easytier::proto::rpc_types::controller::BaseController;
use easytier::web_client::{self, WebClient};
use easytier::{
    common::config::{NetworkConfig, NetworkConfigExt, load_config_from_file},
    common::{
        MachineIdOptions,
        config::{ConfigLoader, ConfigSource, FileLoggerConfig, LoggingConfig, TomlConfigLoader},
        log, resolve_machine_id,
    },
    instance::factory::{NativeInstanceManager, native_instance_manager_with_config_dir},
    proto::rpc::standalone::{runtime_rpc_dialer, runtime_rpc_listener},
    rpc_service::ApiRpcServer,
    utils::panic::setup_panic_handler,
};
use easytier_core::management::InstanceStateStore;
use easytier_core::management::config_source_to_rpc;
use easytier_core::management::remote_client::{
    GetNetworkMetasResponse, ListNetworkInstanceIdsJsonResp, ListNetworkProps, RemoteClientManager,
    Storage,
};
use easytier_core::{
    connectivity::protocol::raw::TunnelDialer as _, process_runtime::CoreProcessRuntime,
    socket::SocketListener, tunnel::Tunnel,
};
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard};
use uuid::Uuid;

use tauri::{AppHandle, Emitter, Manager as _};

#[cfg(not(target_os = "android"))]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

static INSTANCE_MANAGER: once_cell::sync::Lazy<RwLock<Option<Arc<NativeInstanceManager>>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(None));

/// Directory under the app data dir where the embedded core persists managed
/// network configs (`{instance_id}.toml`) and the enabled/disabled state file.
/// Setting it opts the GUI into the decentralized management mode.
static INSTANCE_CONFIG_DIR: once_cell::sync::Lazy<RwLock<Option<std::path::PathBuf>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(None));

static INSTANCE_STATE_STORE: once_cell::sync::Lazy<RwLock<Option<Arc<InstanceStateStore>>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(None));

static RPC_RING_UUID: once_cell::sync::Lazy<uuid::Uuid> =
    once_cell::sync::Lazy::new(uuid::Uuid::new_v4);

static CLIENT_MANAGER: once_cell::sync::Lazy<RwLock<Option<manager::GUIClientManager>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(None));

static LOCAL_GUI_STORAGE: once_cell::sync::Lazy<Arc<manager::GUIStorage>> =
    once_cell::sync::Lazy::new(|| Arc::new(manager::GUIStorage::new(None)));

type BoxedTunnelListener = Box<dyn SocketListener<Accepted = Box<dyn Tunnel>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RpcServerKind {
    Ring,
    Tcp,
}

struct RpcServer {
    kind: RpcServerKind,
    _server: ApiRpcServer<BoxedTunnelListener>,
    bind_url: Option<url::Url>,
}
static RPC_SERVER: once_cell::sync::Lazy<Mutex<Option<RpcServer>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(None));

/// One web client per configured config server URL. Multiple addresses are
/// accepted as a comma-separated string, mirroring the core CLI's
/// `--config-server a,b,c` behavior.
static WEB_CLIENT: once_cell::sync::Lazy<RwLock<Vec<WebClient>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(Vec::new()));

/// Prevent a reconnect, mode switch and web-client replacement from racing.
static BACKEND_LIFECYCLE: once_cell::sync::Lazy<Mutex<()>> =
    once_cell::sync::Lazy::new(|| Mutex::new(()));

/// Splits a comma-separated config server list into trimmed, non-empty URLs.
fn parse_config_server_urls(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
        .collect()
}

macro_rules! get_client_manager {
    () => {{
        let guard = CLIENT_MANAGER
            .try_read()
            .map_err(|_| "Failed to acquire read lock for client manager")?;
        RwLockReadGuard::try_map(guard, |cm| cm.as_ref())
            .map_err(|_| "RPC connection not initialized".to_string())
    }};
}

mod persisted_configs;

#[tauri::command]
fn easytier_version() -> Result<String, String> {
    Ok(easytier::VERSION.to_string())
}

#[tauri::command]
fn set_dock_visibility(app: tauri::AppHandle, visible: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        app.set_activation_policy(if visible {
            ActivationPolicy::Regular
        } else {
            ActivationPolicy::Accessory
        })
        .map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, visible);
    Ok(())
}

#[tauri::command]
fn parse_network_config(cfg: NetworkConfig) -> Result<String, String> {
    let toml = cfg.gen_config().map_err(|e| e.to_string())?;
    Ok(toml.dump())
}

#[tauri::command]
fn generate_network_config(toml_config: String) -> Result<NetworkConfig, String> {
    let config = TomlConfigLoader::new_from_str(&toml_config).map_err(|e| e.to_string())?;
    let cfg = NetworkConfig::new_from_config(&config).map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
async fn run_network_instance(
    app: AppHandle,
    cfg: NetworkConfig,
    save: bool,
) -> Result<(), String> {
    let client_manager = get_client_manager!()?;
    let _mutation = client_manager.config_mutation.lock().await;
    client_manager
        .handle_run_network_instance(app, cfg, save)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn collect_network_info(
    app: AppHandle,
    instance_id: String,
) -> Result<CollectNetworkInfoResponse, String> {
    let instance_id = instance_id
        .parse()
        .map_err(|e: uuid::Error| e.to_string())?;
    get_client_manager!()?
        .handle_collect_network_info(app, Some(vec![instance_id]))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_vpn_portal_info(instance_id: String) -> Result<Option<VpnPortalInfo>, String> {
    let instance_id = instance_id
        .parse::<uuid::Uuid>()
        .map_err(|e| e.to_string())?;
    let client_manager = get_client_manager!()?;
    let client = client_manager
        .rpc_manager
        .rpc_client()
        .scoped_client::<VpnPortalRpcClientFactory<BaseController>>(1, 1, "".to_string());
    let response = client
        .get_vpn_portal_info(
            BaseController::default(),
            GetVpnPortalInfoRequest {
                instance: Some(InstanceIdentifier {
                    selector: Some(instance_identifier::Selector::Id(instance_id.into())),
                }),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(response.vpn_portal_info)
}

#[tauri::command]
async fn patch_vpn_portal_clients(
    app: AppHandle,
    instance_id: String,
    action: String,
    name: Option<String>,
    virtual_ip: Option<String>,
    groups: Option<Vec<String>>,
) -> Result<(), String> {
    let instance_id = instance_id
        .parse::<uuid::Uuid>()
        .map_err(|e| e.to_string())?;
    let action = match action.as_str() {
        "add" => ConfigPatchAction::Add,
        "remove" => ConfigPatchAction::Remove,
        "clear" => ConfigPatchAction::Clear,
        other => return Err(format!("invalid vpn portal client patch action: {other}")),
    };
    let client = if action == ConfigPatchAction::Clear {
        None
    } else {
        Some(VpnPortalClientConfig {
            name: name.unwrap_or_default(),
            virtual_ip: virtual_ip.unwrap_or_default(),
            groups: groups.unwrap_or_default(),
        })
    };

    let client_manager = get_client_manager!()?;
    let _mutation = client_manager.config_mutation.lock().await;
    client_manager
        .handle_patch_vpn_portal_clients(
            app,
            instance_id,
            vec![VpnPortalClientPatch {
                action: action as i32,
                client,
            }],
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_logging_level(level: String) -> Result<(), String> {
    get_client_manager!()?
        .set_logging_level(level.clone())
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn set_tun_fd(fd: i32) -> Result<(), String> {
    let Some(instance_manager) = INSTANCE_MANAGER.read().await.clone() else {
        return Err("set_tun_fd is not supported in remote mode".to_string());
    };
    if let Some(uuid) = instance_manager.instance_ids().into_iter().find(|id| {
        instance_manager
            .config(*id)
            .is_some_and(|config| !config.get_flags().no_tun)
    }) {
        instance_manager
            .attach_tun_fd(uuid, fd)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn list_network_instance_ids(
    app: AppHandle,
) -> Result<ListNetworkInstanceIdsJsonResp, String> {
    get_client_manager!()?
        .handle_list_network_instance_ids(app)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn remove_network_instance(
    app: AppHandle,
    instance_id: String,
    expected_revision: Option<String>,
) -> Result<(), String> {
    if let Some(revision) = expected_revision {
        return persisted_configs::remove_local_config(instance_id, revision).await;
    }
    let instance_id = instance_id
        .parse()
        .map_err(|e: uuid::Error| e.to_string())?;
    let client_manager = get_client_manager!()?;
    let _mutation = client_manager.config_mutation.lock().await;
    client_manager
        .handle_remove_network_instances(app.clone(), vec![instance_id])
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn update_network_config_state(
    app: AppHandle,
    instance_id: String,
    disabled: bool,
    expected_revision: Option<String>,
) -> Result<(), String> {
    if let Some(revision) = expected_revision {
        return persisted_configs::set_local_config_enabled(instance_id, revision, !disabled).await;
    }
    let instance_id = instance_id
        .parse()
        .map_err(|e: uuid::Error| e.to_string())?;
    let client_manager = get_client_manager!()?;
    let _mutation = client_manager.config_mutation.lock().await;
    client_manager
        .handle_update_network_state(app, instance_id, disabled)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
async fn save_network_config(app: AppHandle, cfg: NetworkConfig) -> Result<(), String> {
    let instance_id = cfg
        .instance_id()
        .parse()
        .map_err(|e: uuid::Error| e.to_string())?;
    let client_manager = get_client_manager!()?;
    let _mutation = client_manager.config_mutation.lock().await;
    client_manager
        .handle_save_network_config(app, instance_id, cfg)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn validate_config(
    app: AppHandle,
    config: NetworkConfig,
) -> Result<ValidateConfigResponse, String> {
    get_client_manager!()?
        .handle_validate_config(app, config)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_config(app: AppHandle, instance_id: String) -> Result<NetworkConfig, String> {
    let instance_id = instance_id
        .parse()
        .map_err(|e: uuid::Error| e.to_string())?;
    let cfg = get_client_manager!()?
        .handle_get_network_config(app, instance_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
async fn load_configs(
    app: AppHandle,
    configs: Vec<manager::StoredGuiConfig>,
    enabled_networks: Vec<String>,
) -> Result<(), String> {
    let client_manager = get_client_manager!()?;
    let _mutation = client_manager.config_mutation.lock().await;
    client_manager
        .load_configs(app, configs, enabled_networks)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn get_network_metas(
    app: AppHandle,
    instance_ids: Vec<uuid::Uuid>,
) -> Result<GetNetworkMetasResponse, String> {
    get_client_manager!()?
        .handle_get_network_metas(app, instance_ids)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "android")]
#[tauri::command]
fn init_service() -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn init_service(app: AppHandle, opts: Option<service::ServiceOptions>) -> Result<(), String> {
    // SCM calls can block for seconds; run them on the blocking pool instead
    // of the async runtime / main thread.
    match opts {
        Some(mut args) => {
            args.config_dir = shared_config_dir(&app, Some(&args.config_dir))?
                .to_string_lossy()
                .to_string();
            args.rpc_portal = service_conflicts::normalize_rpc_portal(&args.rpc_portal)
                .map_err(|error| format!("{error:#}"))?;
            args.machine_id = Some(gui_machine_id(&app)?.to_string());
            let path = std::path::Path::new(&args.file_log_dir);
            if !path.exists() {
                std::fs::create_dir_all(&args.file_log_dir).map_err(|e| e.to_string())?;
            } else if !path.is_dir() {
                return Err("file_log_dir exists but is not a directory".to_string());
            }

            tokio::task::spawn_blocking(move || service::install(args))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("{:#}", e))?;
        }
        None => {
            tokio::task::spawn_blocking(service::uninstall)
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("{:#}", e))?;
        }
    }
    Ok(())
}

#[tauri::command]
async fn set_service_status(enable: bool, rpc_portal: Option<String>) -> Result<(), String> {
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
async fn retire_conflicting_services(
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

fn shared_config_dir(
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
fn resolve_shared_config_dir(app: AppHandle, config_dir: Option<String>) -> Result<String, String> {
    Ok(shared_config_dir(&app, config_dir.as_deref())?
        .to_string_lossy()
        .to_string())
}

fn gui_machine_id(app: &AppHandle) -> Result<Uuid, String> {
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

async fn restore_managed_configs(
    manager: &NativeInstanceManager,
    state_store: &InstanceStateStore,
    config_dir: &std::path::PathBuf,
) -> anyhow::Result<()> {
    let mut configs = Vec::new();
    for entry in std::fs::read_dir(config_dir)? {
        let path = entry?.path();
        if path.is_file() && path.extension() == Some(std::ffi::OsStr::new("toml")) {
            configs.push(path);
        }
    }
    configs.sort();

    for path in configs {
        let (config, control) = load_config_from_file(&path, Some(config_dir), false).await?;
        manager.register_managed_config(config.get_id(), control.clone())?;
        if state_store.is_enabled(&config.get_id()) {
            manager.run_network_instance(config, control)?;
        }
    }

    // Match the service core: a removed config cannot remain disabled forever.
    let seen = manager.managed_config_ids();
    for id in state_store.disabled_instance_ids() {
        if !seen.contains(&id) {
            state_store.remove(&id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod managed_restore_tests {
    use super::*;

    #[test]
    fn normal_mode_accepts_a_saved_service_rpc_address() {
        let (bind, connect) = normalize_normal_mode_rpc_portal("127.0.0.1:15999").unwrap();
        assert_eq!(bind.scheme(), "tcp");
        assert_eq!(bind.host_str(), Some("127.0.0.1"));
        assert_eq!(bind.port(), Some(15999));
        assert_eq!(connect, bind);
    }

    #[test]
    fn normal_mode_normalizes_wildcard_connect_addrs() {
        let (bind, connect) = normalize_normal_mode_rpc_portal("0.0.0.0:15999").unwrap();
        assert_eq!(bind.host_str(), Some("0.0.0.0"));
        assert_eq!(connect.host_str(), Some("127.0.0.1"));

        let (bind, connect) = normalize_normal_mode_rpc_portal("tcp://[::]:15999").unwrap();
        assert_eq!(bind.host_str(), Some("[::]"));
        assert_eq!(connect.host_str(), Some("[::1]"));

        // The GUI never connects out via a wildcard addr; the connect URL
        // must be dialable.
        assert!(connect.socket_addrs(|| None).is_ok());
    }

    #[tokio::test]
    async fn normal_mode_restores_the_service_config_dir_state() {
        let dir = tempfile::tempdir().unwrap();
        let enabled = TomlConfigLoader::default();
        enabled.set_inst_name("enabled".to_string());
        enabled.set_listeners(Vec::new());
        let mut enabled_flags = enabled.get_flags();
        enabled_flags.no_tun = true;
        enabled.set_flags(enabled_flags);
        let enabled_id = enabled.get_id();
        std::fs::write(
            dir.path().join(format!("{enabled_id}.toml")),
            enabled.dump(),
        )
        .unwrap();

        let disabled = TomlConfigLoader::default();
        disabled.set_inst_name("disabled".to_string());
        disabled.set_listeners(Vec::new());
        let mut disabled_flags = disabled.get_flags();
        disabled_flags.no_tun = true;
        disabled.set_flags(disabled_flags);
        let disabled_id = disabled.get_id();
        std::fs::write(dir.path().join("named-disabled.toml"), disabled.dump()).unwrap();

        let stale_id = Uuid::new_v4();
        let state_store = InstanceStateStore::new(Some(dir.path()));
        state_store.set_enabled(disabled_id, false).unwrap();
        state_store.set_enabled(stale_id, false).unwrap();
        drop(state_store);

        let state_store = InstanceStateStore::new(Some(dir.path()));
        let manager = native_instance_manager_with_config_dir(Some(dir.path().to_path_buf()));
        restore_managed_configs(&manager, &state_store, &dir.path().to_path_buf())
            .await
            .unwrap();

        assert!(manager.instance_ids().contains(&enabled_id));
        assert!(!manager.instance_ids().contains(&disabled_id));
        assert_eq!(state_store.disabled_instance_ids(), vec![disabled_id]);
        assert_eq!(
            manager.managed_config_control(disabled_id).unwrap().path,
            Some(dir.path().join("named-disabled.toml"))
        );
        manager.retain_network_instances(&[]).await.unwrap();
    }
}

async fn stop_local_backend_inner() -> Result<(), String> {
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
    let manager = INSTANCE_MANAGER.read().await.clone();
    if let Some(manager) = manager {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            manager.retain_network_instances(&[]),
        )
        .await
        .map_err(|_| "Timed out waiting for local network instances to stop".to_string())?
        .map_err(|error| error.to_string())?;
    }
    *INSTANCE_MANAGER.write().await = None;
    *INSTANCE_STATE_STORE.write().await = None;
    *INSTANCE_CONFIG_DIR.write().await = None;
    LOCAL_GUI_STORAGE.clear();
    Ok(())
}

/// Release the embedded core before a service claims its ports and TUN devices.
#[tauri::command]
async fn stop_local_backend() -> Result<(), String> {
    let _lifecycle = BACKEND_LIFECYCLE.lock().await;
    stop_local_backend_inner().await
}

#[tauri::command]
async fn get_service_status() -> Result<&'static str, String> {
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

fn normalize_normal_mode_rpc_portal(portal: &str) -> Result<(url::Url, url::Url), String> {
    let portal = if portal.contains("://") {
        portal.to_string()
    } else {
        format!("tcp://{portal}")
    };
    let portal_url: url::Url = portal
        .parse()
        .map_err(|e| format!("invalid rpc portal: {:#}", e))?;
    let bind_url = portal_url.clone();
    let mut connect_url = portal_url.clone();
    // Wildcard bind addrs are not valid connect targets; dial loopback
    // instead. tcp:// is a non-special scheme for the url crate, so wildcard
    // literals can arrive as Domain("[::]") as well as Ipv4/Ipv6 hosts.
    match connect_url.host() {
        Some(url::Host::Ipv4(ip)) if ip.is_unspecified() => {
            connect_url.set_host(Some("127.0.0.1")).unwrap();
        }
        Some(url::Host::Ipv6(ip)) if ip.is_unspecified() => {
            connect_url.set_host(Some("[::1]")).unwrap();
        }
        _ => match connect_url.host_str() {
            Some("0.0.0.0") => {
                connect_url.set_host(Some("127.0.0.1")).unwrap();
            }
            // The bare "::" and bracketed forms appear for non-special
            // schemes (and user-typed portals); both mean IPv6 wildcard.
            Some("::") | Some("[::]") => {
                connect_url.set_host(Some("[::1]")).unwrap();
            }
            _ => {}
        },
    }
    Ok((bind_url, connect_url))
}

async fn resolve_rpc_bind_url(url: &url::Url) -> Result<std::net::SocketAddr, String> {
    if url.scheme() != "tcp" {
        return Err(format!("RPC portal requires tcp URL: {url}"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("RPC portal has no host: {url}"))?;
    let port = url.port().unwrap_or(11010);
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("failed to resolve RPC portal {url}: {error}"))?
        .next()
        .ok_or_else(|| format!("RPC portal has no resolved address: {url}"))
}

#[tauri::command]
async fn init_rpc_connection(
    app: AppHandle,
    is_normal_mode: bool,
    url: Option<String>,
    config_dir: Option<String>,
    remote_config_cache: Option<bool>,
) -> Result<(), String> {
    let _lifecycle = BACKEND_LIFECYCLE.lock().await;
    let desired_config_dir = if is_normal_mode {
        Some(shared_config_dir(&app, config_dir.as_deref())?)
    } else {
        None
    };
    let current_config_dir = INSTANCE_CONFIG_DIR.read().await.clone();
    if !is_normal_mode || (current_config_dir.is_some() && current_config_dir != desired_config_dir)
    {
        stop_local_backend_inner().await?;
    } else {
        let client = CLIENT_MANAGER.write().await.take();
        if let Some(client) = client {
            client.rpc_manager.stop().await;
        }
    }

    let mut instance_manager_guard = INSTANCE_MANAGER.write().await;
    let mut rpc_server_guard = RPC_SERVER.lock().await;

    let mut client_url = url.clone();
    let mut local_process_runtime = None;
    if is_normal_mode {
        let config_dir = desired_config_dir.expect("normal mode has a config dir");
        *INSTANCE_CONFIG_DIR.write().await = Some(config_dir.clone());
        let instance_state_store = {
            let mut store_guard = INSTANCE_STATE_STORE.write().await;
            if store_guard.is_none() {
                *store_guard = Some(Arc::new(InstanceStateStore::new(Some(&config_dir))));
            }
            store_guard.clone().unwrap()
        };

        let instance_manager = if let Some(im) = instance_manager_guard.as_ref() {
            im.clone()
        } else {
            let manager = Arc::new(native_instance_manager_with_config_dir(Some(
                config_dir.clone(),
            )));
            if let Err(error) =
                restore_managed_configs(&manager, &instance_state_store, &config_dir).await
            {
                let _ = manager.retain_network_instances(&[]).await;
                *INSTANCE_STATE_STORE.write().await = None;
                *INSTANCE_CONFIG_DIR.write().await = None;
                return Err(format!("Failed to restore managed configs: {error:#}"));
            }
            *instance_manager_guard = Some(manager.clone());
            manager
        };

        let portal = url.and_then(|s| {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });

        let (desired_kind, bind_url, connect_url) = if let Some(portal) = portal {
            let (bind_url, connect_url) = normalize_normal_mode_rpc_portal(&portal)?;
            (RpcServerKind::Tcp, Some(bind_url), Some(connect_url))
        } else {
            (RpcServerKind::Ring, None, None)
        };

        let need_restart = rpc_server_guard
            .as_ref()
            .map(|x| x.kind != desired_kind || x.bind_url != bind_url)
            .unwrap_or(true);

        if need_restart {
            if let Some(mut server) = rpc_server_guard.take() {
                server._server.shutdown().await;
            }

            let tunnel: BoxedTunnelListener = match desired_kind {
                RpcServerKind::Ring => instance_manager
                    .process_runtime()
                    .bind_ring_tunnel(*RPC_RING_UUID.deref())
                    .map_err(|error| error.to_string())?,
                RpcServerKind::Tcp => {
                    let bind_url = bind_url.as_ref().expect("tcp rpc must have bind url");
                    Box::new(runtime_rpc_listener(resolve_rpc_bind_url(bind_url).await?))
                }
            };

            let hooks = Arc::new(manager::GuiHooks {
                app: app.clone(),
                storage: LOCAL_GUI_STORAGE.clone(),
                instances: instance_manager.clone(),
            });
            let rpc_server = ApiRpcServer::from_tunnel_with_hooks(
                tunnel,
                instance_manager.clone(),
                instance_state_store,
                hooks,
            )
            .with_rx_timeout(None)
            .serve()
            .await
            .map_err(|e| e.to_string())?;
            *rpc_server_guard = Some(RpcServer {
                kind: desired_kind,
                _server: rpc_server,
                bind_url,
            });
        }

        local_process_runtime = Some(instance_manager.process_runtime());
        client_url = connect_url.map(|u| u.to_string());
    }

    let client_manager = tokio::time::timeout(
        std::time::Duration::from_millis(1000),
        manager::GUIClientManager::new(
            client_url,
            local_process_runtime,
            remote_config_cache.unwrap_or(false),
        ),
    )
    .await
    .map_err(|_| "connect remote rpc timed out".to_string())?
    .with_context(|| "Failed to connect remote rpc")
    .map_err(|e| format!("{:#}", e))?;
    *CLIENT_MANAGER.write().await = Some(client_manager);

    Ok(())
}

#[tauri::command]
async fn is_client_running() -> Result<bool, String> {
    Ok(CLIENT_MANAGER
        .read()
        .await
        .as_ref()
        .is_some_and(|client| client.rpc_manager.is_running()))
}

#[tauri::command]
async fn init_web_client(app: AppHandle, url: Option<String>) -> Result<(), String> {
    let _lifecycle = BACKEND_LIFECYCLE.lock().await;
    let config_server_urls = url
        .as_deref()
        .map(parse_config_server_urls)
        .unwrap_or_default();
    let (instance_manager, state_store, machine_id) = if config_server_urls.is_empty() {
        (None, None, None)
    } else {
        let manager = INSTANCE_MANAGER
            .read()
            .await
            .clone()
            .ok_or_else(|| "Instance manager is not available".to_string())?;
        let state_store = INSTANCE_STATE_STORE
            .read()
            .await
            .clone()
            .ok_or_else(|| "Instance state store is not available".to_string())?;
        (
            Some(manager),
            Some(state_store),
            Some(gui_machine_id(&app)?),
        )
    };

    let old_clients = std::mem::take(&mut *WEB_CLIENT.write().await);
    for client in old_clients {
        client.shutdown().await;
    }
    let (Some(instance_manager), Some(state_store), Some(machine_id)) =
        (instance_manager, state_store, machine_id)
    else {
        return Ok(());
    };

    let hooks = Arc::new(manager::GuiHooks {
        app,
        storage: LOCAL_GUI_STORAGE.clone(),
        instances: instance_manager.clone(),
    });
    let mut new_clients = Vec::new();
    for config_server_url in config_server_urls {
        let result = web_client::run_web_client(
            config_server_url.as_str(),
            MachineIdOptions {
                explicit_machine_id: Some(machine_id.to_string()),
                state_dir: None,
            },
            None,
            false,
            instance_manager.clone(),
            Some(hooks.clone()),
            state_store.clone(),
        )
        .await;
        match result {
            Ok(client) => new_clients.push(client),
            Err(error) => {
                for client in new_clients {
                    client.shutdown().await;
                }
                return Err(format!(
                    "Failed to initialize web client for {config_server_url}: {error:#}"
                ));
            }
        }
    }
    *WEB_CLIENT.write().await = new_clients;
    Ok(())
}

#[tauri::command]
async fn is_web_client_connected() -> Result<bool, String> {
    let web_clients = WEB_CLIENT.read().await;
    Ok(!web_clients.is_empty() && web_clients.iter().all(|client| client.is_connected()))
}

// 获取日志目录的辅助函数
fn get_log_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, tauri::Error> {
    if cfg!(target_os = "android") {
        // Android: cache_dir + logs 子目录
        app.path().cache_dir().map(|p| p.join("logs"))
    } else {
        // 其他平台: 默认日志目录
        app.path().app_log_dir()
    }
}

#[tauri::command]
async fn get_log_dir_path(app: tauri::AppHandle) -> Result<String, String> {
    match get_log_dir(&app) {
        Ok(log_dir) => {
            std::fs::create_dir_all(&log_dir).ok();
            Ok(log_dir.to_string_lossy().to_string())
        }
        Err(e) => Err(format!("Failed to get log directory: {}", e)),
    }
}

#[cfg(not(target_os = "android"))]
fn toggle_window_visibility(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let visible = window.is_visible().unwrap_or_default();
        let minimized = window.is_minimized().unwrap_or_default();
        let focused = window.is_focused().unwrap_or_default();

        let should_show = !visible || minimized || !focused;
        if should_show {
            if !visible {
                let _ = window.show();
            }
            if minimized {
                let _ = window.unminimize();
            }
            if !focused {
                let _ = window.set_focus();
            }
            let _ = set_dock_visibility(app.clone(), true);
        } else {
            let _ = window.hide();
            let _ = set_dock_visibility(app.clone(), false);
        }
    }
}

fn get_exe_path() -> String {
    if let Ok(appimage_path) = std::env::var("APPIMAGE")
        && !appimage_path.is_empty()
    {
        return appimage_path;
    }
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

#[cfg(not(target_os = "android"))]
fn check_sudo() -> bool {
    let is_elevated = elevate::Command::is_elevated();
    if !is_elevated {
        let exe_path = get_exe_path();
        let stdcmd = std::process::Command::new(&exe_path);
        elevate::Command::new(stdcmd)
            .output()
            .expect("Failed to run elevated command");
    }
    is_elevated
}

mod manager {
    use super::*;
    use async_trait::async_trait;
    use dashmap::{DashMap, DashSet};
    use easytier::common::config::{NetworkConfig, NetworkConfigExt};
    use easytier::proto::api::logger::{LoggerRpc, LoggerRpcClientFactory, SetLoggerConfigRequest};
    use easytier::proto::api::manage::{
        DeleteNetworkInstanceRequest, GetNetworkInstanceConfigRequest, ListNetworkInstanceRequest,
        RemoveNetworkInstanceConfigRequest, RunNetworkInstanceRequest,
        SaveNetworkInstanceConfigRequest, SetNetworkInstanceEnabledRequest,
    };
    use easytier::proto::rpc::bidirect::BidirectRpcManager;
    use easytier::proto::rpc_types::controller::BaseController;
    use easytier::proto::rpc_types::error::Error as RpcError;
    use easytier::web_client::WebClientHooks;
    use easytier_core::management::ActiveInstanceForStart;
    use easytier_core::management::remote_client::{PersistentConfig, RemoteClientError};

    pub(super) struct GuiHooks {
        pub(super) app: AppHandle,
        pub(super) storage: Arc<GUIStorage>,
        pub(super) instances: Arc<NativeInstanceManager>,
    }

    impl GuiHooks {
        fn notify_vpn_stop_if_no_tun(&self) -> Result<(), String> {
            let has_tun = self.instances.instance_ids().into_iter().any(|id| {
                self.instances
                    .config(id)
                    .is_some_and(|config| !config.get_flags().no_tun)
            });
            if !has_tun {
                self.app
                    .emit("vpn_service_stop", "")
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        }
    }

    #[cfg(any(test, target_os = "android"))]
    fn tun_instances_to_stop(
        id: Uuid,
        no_tun: bool,
        source: ConfigSource,
        active: &[ActiveInstanceForStart],
    ) -> Result<Vec<Uuid>, String> {
        if no_tun {
            return Ok(Vec::new());
        }
        let other_tun = active
            .iter()
            .filter(|instance| instance.instance_id != id && !instance.no_tun);
        if source == ConfigSource::Web
            && active
                .iter()
                .any(|instance| !instance.no_tun && instance.source != ConfigSource::Web)
        {
            return Err(
                "Android only supports one active TUN network; user-managed VPN remains active"
                    .to_string(),
            );
        }
        Ok(other_tun.map(|instance| instance.instance_id).collect())
    }

    #[async_trait]
    impl WebClientHooks for GuiHooks {
        async fn prepare_start(
            &self,
            cfg: &TomlConfigLoader,
            active: &[ActiveInstanceForStart],
        ) -> Result<Vec<Uuid>, String> {
            #[cfg(target_os = "android")]
            {
                tun_instances_to_stop(
                    cfg.get_id(),
                    cfg.get_flags().no_tun,
                    cfg.get_network_config_source(),
                    active,
                )
            }
            #[cfg(not(target_os = "android"))]
            {
                let _ = (cfg, active);
                Ok(Vec::new())
            }
        }

        async fn pre_run_network_instance(
            &self,
            cfg: &easytier::common::config::TomlConfigLoader,
        ) -> Result<(), String> {
            self.storage
                .save_config(
                    &self.app,
                    cfg.get_id(),
                    NetworkConfig::new_from_config(cfg).map_err(|e| e.to_string())?,
                    PersistedConfigSource::from_runtime_source(cfg.get_network_config_source()),
                )
                .map_err(|e| e.to_string())?;
            self.app
                .emit("pre_run_network_instance", cfg.get_id().to_string())
                .map_err(|e| e.to_string())
        }

        async fn post_run_network_instance(&self, instance_id: &uuid::Uuid) -> Result<(), String> {
            #[cfg(target_os = "android")]
            if let Some(instance) = self.instances.instance(*instance_id)
                && let Some(mut receiver) = subscribe_native_instance_event(&instance)
            {
                let app = self.app.clone();
                let id = instance_id.to_string();
                tokio::spawn(async move {
                    loop {
                        let event = match receiver.recv().await {
                            Ok(easytier::common::global_ctx::GlobalCtxEvent::DhcpIpv4Changed(
                                _,
                                _,
                            )) => "dhcp_ip_changed",
                            Ok(
                                easytier::common::global_ctx::GlobalCtxEvent::ProxyCidrsUpdated(
                                    _,
                                    _,
                                ),
                            ) => "proxy_cidrs_updated",
                            Ok(_) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                receiver = receiver.resubscribe();
                                "event_lagged"
                            }
                        };
                        let _ = app.emit(event, &id);
                    }
                });
            }
            self.storage.enabled_networks.insert(*instance_id);
            self.storage
                .save_enabled_networks(&self.app)
                .map_err(|e| e.to_string())?;
            self.app
                .emit("post_run_network_instance", instance_id.to_string())
                .map_err(|e| e.to_string())
        }

        async fn post_stop_network_instances(&self, ids: &[Uuid]) -> Result<(), String> {
            for id in ids {
                self.storage.enabled_networks.remove(id);
            }
            self.storage
                .save_enabled_networks(&self.app)
                .map_err(|e| e.to_string())?;
            self.notify_vpn_stop_if_no_tun()
        }

        async fn post_remove_network_instances(&self, ids: &[uuid::Uuid]) -> Result<(), String> {
            self.storage
                .delete_network_configs(self.app.clone(), ids)
                .await
                .map_err(|e| e.to_string())?;
            self.notify_vpn_stop_if_no_tun()
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    #[derive(Default)]
    pub(super) enum PersistedConfigSource {
        User,
        #[serde(alias = "webhook")]
        Web,
        #[serde(other)]
        #[default]
        Legacy,
    }

    impl PersistedConfigSource {
        pub(super) fn from_runtime_source(source: ConfigSource) -> Self {
            match source {
                ConfigSource::User => Self::User,
                ConfigSource::Web => Self::Web,
            }
        }

        fn merge_persisted(self, incoming: Self) -> Self {
            match (self, incoming) {
                // Older runtimes report missing source as `user`. Keep the stronger persisted
                // ownership until web sync or an explicit user save repairs it.
                (Self::Web, Self::User) | (Self::Legacy, Self::User) => self,
                (_, next) => next,
            }
        }

        fn to_runtime_source(self) -> ConfigSource {
            match self {
                Self::User | Self::Legacy => ConfigSource::User,
                Self::Web => ConfigSource::Web,
            }
        }

        #[cfg(test)]
        fn is_web_like(self) -> bool {
            matches!(self, Self::Web)
        }
    }

    #[derive(Clone)]
    pub(super) struct GUIConfig {
        inst_id: String,
        pub(crate) config: NetworkConfig,
        source: PersistedConfigSource,
    }

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    pub(super) struct StoredGuiConfig {
        config: NetworkConfig,
        #[serde(default)]
        source: PersistedConfigSource,
    }

    impl GUIConfig {
        fn new(inst_id: String, config: NetworkConfig, source: PersistedConfigSource) -> Self {
            Self {
                inst_id,
                config,
                source,
            }
        }

        fn into_stored(self) -> StoredGuiConfig {
            StoredGuiConfig {
                config: self.config,
                source: self.source,
            }
        }
    }

    impl PersistentConfig<anyhow::Error> for GUIConfig {
        fn get_network_inst_id(&self) -> &str {
            &self.inst_id
        }
        fn get_network_config(&self) -> Result<NetworkConfig, anyhow::Error> {
            Ok(self.config.clone())
        }
        fn get_network_config_source(&self) -> ConfigSource {
            self.source.to_runtime_source()
        }
    }

    pub(super) struct GUIStorage {
        network_configs: DashMap<Uuid, GUIConfig>,
        enabled_networks: DashSet<Uuid>,
        fallback_configs: DashSet<Uuid>,
        remote_rpc_url: Option<String>,
    }
    impl GUIStorage {
        pub(super) fn new(remote_rpc_url: Option<String>) -> Self {
            Self {
                network_configs: DashMap::new(),
                enabled_networks: DashSet::new(),
                fallback_configs: DashSet::new(),
                remote_rpc_url,
            }
        }

        pub(super) fn clear(&self) {
            self.network_configs.clear();
            self.enabled_networks.clear();
            self.fallback_configs.clear();
        }

        fn notify_vpn_stop_if_no_tun(&self, app: &AppHandle) -> Result<(), String> {
            let has_tun = self
                .network_configs
                .iter()
                .any(|entry| self.enabled_networks.contains(entry.key()) && !entry.config.no_tun());
            if !has_tun {
                app.emit("vpn_service_stop", "")
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        }

        fn remove_cached_configs(&self, ids: &[Uuid]) {
            for id in ids {
                self.network_configs.remove(id);
                self.enabled_networks.remove(id);
                self.fallback_configs.remove(id);
            }
        }

        fn save_configs(&self, app: &AppHandle) -> anyhow::Result<()> {
            let configs = self
                .network_configs
                .iter()
                .map(|entry| entry.value().clone().into_stored())
                .collect::<Vec<_>>();
            if let Some(rpc_url) = &self.remote_rpc_url {
                #[derive(Clone, serde::Serialize)]
                struct RemoteConfigs<'a> {
                    rpc_url: &'a str,
                    configs: Vec<StoredGuiConfig>,
                }
                let fallback = self
                    .network_configs
                    .iter()
                    .filter(|entry| self.fallback_configs.contains(entry.key()))
                    .map(|entry| entry.value().clone().into_stored())
                    .collect();
                app.emit(
                    "save_remote_configs",
                    RemoteConfigs {
                        rpc_url,
                        configs: fallback,
                    },
                )?;
            } else {
                app.emit("save_configs", configs)?;
            }
            Ok(())
        }

        fn save_enabled_networks(&self, app: &AppHandle) -> anyhow::Result<()> {
            if self.remote_rpc_url.is_some() {
                return Ok(());
            }
            let payload: Vec<String> = self
                .enabled_networks
                .iter()
                .map(|entry| entry.key().to_string())
                .collect();
            app.emit("save_enabled_networks", payload)?;
            Ok(())
        }

        fn save_config(
            &self,
            app: &AppHandle,
            inst_id: Uuid,
            cfg: NetworkConfig,
            source: PersistedConfigSource,
        ) -> anyhow::Result<()> {
            let source = self
                .network_configs
                .get(&inst_id)
                .map(|existing| existing.source.merge_persisted(source))
                .unwrap_or(source);
            let config = GUIConfig::new(inst_id.to_string(), cfg, source);
            self.network_configs.insert(inst_id, config);
            self.save_configs(app)
        }
    }
    #[async_trait]
    impl Storage<AppHandle, GUIConfig, anyhow::Error> for GUIStorage {
        async fn insert_or_update_user_network_config(
            &self,
            app: AppHandle,
            network_inst_id: Uuid,
            network_config: NetworkConfig,
            source: ConfigSource,
        ) -> Result<(), anyhow::Error> {
            self.save_config(
                &app,
                network_inst_id,
                network_config,
                PersistedConfigSource::from_runtime_source(source),
            )?;
            self.enabled_networks.insert(network_inst_id);
            self.save_enabled_networks(&app)?;
            Ok(())
        }

        async fn delete_network_configs(
            &self,
            app: AppHandle,
            network_inst_ids: &[Uuid],
        ) -> Result<(), anyhow::Error> {
            self.remove_cached_configs(network_inst_ids);
            self.save_configs(&app)?;
            self.save_enabled_networks(&app)?;
            Ok(())
        }

        async fn update_network_config_state(
            &self,
            app: AppHandle,
            network_inst_id: Uuid,
            disabled: bool,
        ) -> Result<(), anyhow::Error> {
            if disabled {
                self.enabled_networks.remove(&network_inst_id);
            } else {
                self.enabled_networks.insert(network_inst_id);
            }
            self.save_enabled_networks(&app)?;
            Ok(())
        }

        async fn list_network_configs(
            &self,
            _: AppHandle,
            props: ListNetworkProps,
        ) -> Result<Vec<GUIConfig>, anyhow::Error> {
            let mut ret = Vec::new();
            for entry in self.network_configs.iter() {
                let id: Uuid = entry.key().to_owned();
                match props {
                    ListNetworkProps::All => {
                        ret.push(entry.value().clone());
                    }
                    ListNetworkProps::EnabledOnly => {
                        if self.enabled_networks.contains(&id) {
                            ret.push(entry.value().clone());
                        }
                    }
                    ListNetworkProps::DisabledOnly => {
                        if !self.enabled_networks.contains(&id) {
                            ret.push(entry.value().clone());
                        }
                    }
                }
            }
            Ok(ret)
        }

        async fn get_network_config(
            &self,
            _: AppHandle,
            network_inst_id: &str,
        ) -> Result<Option<GUIConfig>, anyhow::Error> {
            let uuid = Uuid::parse_str(network_inst_id)?;
            Ok(self
                .network_configs
                .get(&uuid)
                .map(|entry| entry.value().clone()))
        }
    }

    #[derive(Default)]
    struct RuntimeCapabilities {
        advertised: Option<bool>,
        save: Option<bool>,
        disable: Option<bool>,
        remove: Option<bool>,
    }

    impl RuntimeCapabilities {
        fn observe(&mut self, advertised: Option<bool>) {
            if let Some(supported) = advertised {
                self.advertised = advertised;
                self.save = Some(supported);
                self.disable = Some(supported);
                self.remove = Some(supported);
            }
        }
    }

    fn unsupported_method(error: &RpcError, method: u8) -> bool {
        matches!(error, RpcError::InvalidMethodIndex(index, service)
            if *index == method && (service == "WebClientService"
                || serde_json::from_str::<String>(service).is_ok_and(|name| name == "WebClientService")))
    }

    async fn compatible_rpc_call<T>(
        supported: Option<bool>,
        method: u8,
        call: impl std::future::Future<Output = Result<T, RpcError>>,
    ) -> Result<Option<T>, RpcError> {
        if supported == Some(false) {
            return Ok(None);
        }
        match call.await {
            Ok(value) => Ok(Some(value)),
            Err(error) if unsupported_method(&error, method) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn ensure_removed(
        requested: &[Uuid],
        remaining: &[easytier::proto::common::Uuid],
    ) -> Result<(), RpcError> {
        let retained: Vec<_> = requested
            .iter()
            .filter(|id| remaining.contains(&(**id).into()))
            .collect();
        if !retained.is_empty() {
            return Err(anyhow::anyhow!("Network instances were not removed: {retained:?}").into());
        }
        Ok(())
    }

    fn partition_removed(
        requested: &[Uuid],
        remaining: &[easytier::proto::common::Uuid],
    ) -> (Vec<Uuid>, Vec<Uuid>) {
        requested
            .iter()
            .copied()
            .partition(|id| !remaining.contains(&(*id).into()))
    }

    fn migration_enabled_ids(local: bool, requested: Vec<String>) -> Vec<Uuid> {
        if !local {
            return Vec::new();
        }
        requested
            .into_iter()
            .filter_map(|id| id.parse().ok())
            .collect()
    }

    #[cfg(any(test, target_os = "android"))]
    fn local_vpn_config(
        local: bool,
        config: Option<TomlConfigLoader>,
        prepared: Option<(NetworkConfig, ConfigSource)>,
    ) -> anyhow::Result<Option<(NetworkConfig, ConfigSource)>> {
        if !local {
            return Ok(None);
        }
        let Some(config) = config else {
            return Ok(prepared);
        };
        Ok(Some((
            NetworkConfig::new_from_config(&config)?,
            config.get_network_config_source(),
        )))
    }

    pub(super) struct GUIClientManager {
        pub(super) storage: Arc<GUIStorage>,
        pub(super) rpc_manager: BidirectRpcManager,
        capabilities: std::sync::Mutex<RuntimeCapabilities>,
        local: bool,
        pub(super) config_mutation: Mutex<()>,
    }
    impl GUIClientManager {
        pub async fn new(
            rpc_url: Option<String>,
            local_process_runtime: Option<Arc<CoreProcessRuntime>>,
            remote_config_cache: bool,
        ) -> Result<Self, anyhow::Error> {
            let local = local_process_runtime.is_some();
            let storage = if local {
                LOCAL_GUI_STORAGE.clone()
            } else {
                Arc::new(GUIStorage::new(
                    rpc_url
                        .as_ref()
                        .filter(|_| remote_config_cache)
                        .map(|value| {
                            value
                                .parse::<url::Url>()
                                .map(|url| url.to_string())
                                .unwrap_or_else(|_| value.clone())
                        }),
                ))
            };
            let tunnel = if let Some(url) = rpc_url {
                runtime_rpc_dialer(url.parse()?).connect().await?
            } else {
                local_process_runtime
                    .context("local RPC requires a core process runtime")?
                    .connect_ring_tunnel(*RPC_RING_UUID.deref())?
            };

            let rpc_manager = BidirectRpcManager::new();
            rpc_manager.run_with_tunnel(tunnel);
            let mut capabilities = RuntimeCapabilities::default();
            if local {
                capabilities.observe(Some(true));
            }

            Ok(Self {
                storage,
                rpc_manager,
                capabilities: std::sync::Mutex::new(capabilities),
                local,
                config_mutation: Mutex::new(()),
            })
        }

        pub(super) fn notify_vpn_stop_if_no_tun(&self, app: &AppHandle) -> Result<(), String> {
            self.storage.notify_vpn_stop_if_no_tun(app)
        }

        async fn remove_via_rpc(
            &self,
            client: &mut (dyn WebClientService<Controller = BaseController> + Send),
            ids: &[Uuid],
        ) -> Result<(), RpcError> {
            if ids.is_empty() {
                return Ok(());
            }
            let response = client
                .delete_network_instance(
                    BaseController::default(),
                    DeleteNetworkInstanceRequest {
                        inst_ids: ids.iter().copied().map(Into::into).collect(),

                        ..Default::default()
                    },
                )
                .await?;
            let (mut removed, mut failed) = partition_removed(ids, &response.remain_inst_ids);
            let (advertised, support) = {
                let capabilities = self.capabilities.lock().unwrap();
                (capabilities.advertised, capabilities.remove)
            };
            if advertised != Some(true) && !removed.is_empty() {
                let result = compatible_rpc_call(
                    support,
                    11,
                    client.remove_network_instance_config(
                        BaseController::default(),
                        RemoveNetworkInstanceConfigRequest {
                            inst_ids: removed.iter().copied().map(Into::into).collect(),

                            ..Default::default()
                        },
                    ),
                )
                .await?;
                self.capabilities.lock().unwrap().remove = Some(result.is_some());
                if let Some(response) = result {
                    let (success, retained) =
                        partition_removed(&removed, &response.remain_inst_ids);
                    removed = success;
                    failed.extend(retained);
                }
            }
            if !self.local {
                self.storage.remove_cached_configs(&removed);
            }
            let remaining: Vec<_> = failed.into_iter().map(Into::into).collect();
            ensure_removed(ids, &remaining)
        }

        fn get_logger_rpc_client(
            &self,
        ) -> Option<Box<dyn LoggerRpc<Controller = BaseController> + Send>> {
            Some(
                self.rpc_manager
                    .rpc_client()
                    .scoped_client::<LoggerRpcClientFactory<BaseController>>(1, 1, "".to_string()),
            )
        }

        pub(super) async fn set_logging_level(&self, level: String) -> Result<(), anyhow::Error> {
            let logger_rpc = self
                .get_logger_rpc_client()
                .ok_or_else(|| anyhow::anyhow!("Logger RPC client not available"))?;
            logger_rpc
                .set_logger_config(
                    BaseController::default(),
                    SetLoggerConfigRequest {
                        level: easytier_core::management::parse_log_level(&level).into(),
                    },
                )
                .await?;
            Ok(())
        }

        pub(super) async fn load_configs(
            &self,
            app: AppHandle,
            configs: Vec<StoredGuiConfig>,
            enabled_networks: Vec<String>,
        ) -> anyhow::Result<()> {
            let client = self
                .get_rpc_client(app.clone())
                .ok_or_else(|| anyhow::anyhow!("RPC client not found"))?;
            let existing = client
                .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
                .await?;
            self.capabilities
                .lock()
                .unwrap()
                .observe(existing.supports_persisted_config_management);
            // Only migrate missing legacy configs. The core owns subsequent edits,
            // including disabled configs edited from the web console.
            for stored in configs {
                let id: Uuid = stored.config.instance_id().parse()?;
                let rpc_id = id.into();
                if existing.inst_ids.contains(&rpc_id)
                    || existing.disabled_inst_ids.contains(&rpc_id)
                {
                    if !self.local && self.capabilities.lock().unwrap().advertised != Some(true) {
                        self.storage.fallback_configs.insert(id);
                        self.storage.network_configs.insert(
                            id,
                            GUIConfig::new(id.to_string(), stored.config, stored.source),
                        );
                    } else {
                        self.storage.fallback_configs.remove(&id);
                    }
                    continue;
                }
                if !self.local {
                    // Remote caches are display fallbacks, never desired state
                    // to replay into an official or node-owned configuration.
                    if self.capabilities.lock().unwrap().advertised != Some(true) {
                        self.storage.fallback_configs.insert(id);
                        self.storage.network_configs.insert(
                            id,
                            GUIConfig::new(id.to_string(), stored.config, stored.source),
                        );
                    }
                    continue;
                }
                self.handle_save_network_config_with_source(
                    app.clone(),
                    id,
                    stored.config,
                    stored.source.to_runtime_source(),
                )
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            }
            for id in migration_enabled_ids(self.local, enabled_networks) {
                if self.storage.network_configs.contains_key(&id)
                    || existing.disabled_inst_ids.contains(&id.into())
                {
                    client
                        .set_network_instance_enabled(
                            BaseController::default(),
                            SetNetworkInstanceEnabledRequest {
                                inst_id: Some(id.into()),
                                enabled: true,

                                ..Default::default()
                            },
                        )
                        .await?;
                }
            }
            let snapshot = client
                .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
                .await?;
            self.capabilities
                .lock()
                .unwrap()
                .observe(snapshot.supports_persisted_config_management);
            let mut next: std::collections::HashMap<Uuid, GUIConfig> = self
                .storage
                .network_configs
                .iter()
                .filter(|entry| self.storage.fallback_configs.contains(entry.key()))
                .map(|entry| (*entry.key(), entry.value().clone()))
                .collect();
            for id in snapshot.inst_ids.iter().chain(&snapshot.disabled_inst_ids) {
                let response = match client
                    .get_network_instance_config(
                        BaseController::default(),
                        GetNetworkInstanceConfigRequest { inst_id: Some(*id) },
                    )
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        tracing::warn!(%error, "failed to refresh one network config");
                        if let Some(cached) = self.storage.network_configs.get(&Uuid::from(*id)) {
                            next.insert(*cached.key(), cached.value().clone());
                        }
                        continue;
                    }
                };
                if let Some(config) = response.config {
                    let uuid: Uuid = (*id).into();
                    let source = easytier_core::management::config_source_from_rpc(response.source)
                        .map(PersistedConfigSource::from_runtime_source)
                        .unwrap_or_else(|| {
                            self.storage
                                .network_configs
                                .get(&uuid)
                                .map(|entry| entry.source)
                                .unwrap_or(PersistedConfigSource::Legacy)
                        });
                    next.insert(uuid, GUIConfig::new(uuid.to_string(), config, source));
                }
            }
            self.storage
                .network_configs
                .retain(|id, _| next.contains_key(id));
            for (id, config) in next {
                self.storage.network_configs.insert(id, config);
            }
            self.storage.enabled_networks.clear();
            for id in snapshot.inst_ids {
                self.storage.enabled_networks.insert(id.into());
            }
            self.storage.save_configs(&app)?;
            self.storage.save_enabled_networks(&app)?;
            Ok(())
        }
    }
    #[async_trait]
    impl RemoteClientManager<AppHandle, GUIConfig, anyhow::Error> for GUIClientManager {
        async fn handle_run_network_instance_with_source(
            &self,
            app: AppHandle,
            config: NetworkConfig,
            save: bool,
            source: ConfigSource,
        ) -> Result<(), RemoteClientError<anyhow::Error>> {
            let client = self
                .get_rpc_client(app.clone())
                .ok_or(RemoteClientError::ClientNotFound)?;
            let response = client
                .run_network_instance(
                    BaseController::default(),
                    RunNetworkInstanceRequest {
                        inst_id: None,
                        config: Some(config.clone()),
                        overwrite: true,
                        source: config_source_to_rpc(source),
                    },
                )
                .await?;
            let id: Uuid = response
                .inst_id
                .ok_or_else(|| {
                    RemoteClientError::Other("run response has no instance id".to_string())
                })?
                .into();
            if !self.local {
                if save || self.storage.fallback_configs.contains(&id) {
                    self.storage
                        .save_config(
                            &app,
                            id,
                            config,
                            PersistedConfigSource::from_runtime_source(source),
                        )
                        .map_err(RemoteClientError::PersistentError)?;
                }
                self.storage.enabled_networks.insert(id);
                self.storage
                    .save_enabled_networks(&app)
                    .map_err(RemoteClientError::PersistentError)?;
            }
            Ok(())
        }

        async fn handle_list_network_instance_ids(
            &self,
            app: AppHandle,
        ) -> Result<ListNetworkInstanceIdsJsonResp, RemoteClientError<anyhow::Error>> {
            let client = self
                .get_rpc_client(app)
                .ok_or(RemoteClientError::ClientNotFound)?;
            let response = client
                .list_network_instance(BaseController::default(), ListNetworkInstanceRequest {})
                .await?;
            self.capabilities
                .lock()
                .unwrap()
                .observe(response.supports_persisted_config_management);
            let mut disabled = response.disabled_inst_ids;
            for id in self.storage.fallback_configs.iter() {
                let rpc_id = (*id.key()).into();
                if !response.inst_ids.contains(&rpc_id) && !disabled.contains(&rpc_id) {
                    disabled.push(rpc_id);
                }
            }
            Ok(ListNetworkInstanceIdsJsonResp {
                running_inst_ids: response.inst_ids,
                disabled_inst_ids: disabled,
                runtime_capabilities: response.runtime_capabilities,
            })
        }

        async fn handle_save_network_config_with_source(
            &self,
            app: AppHandle,
            id: Uuid,
            config: NetworkConfig,
            source: ConfigSource,
        ) -> Result<(), RemoteClientError<anyhow::Error>> {
            let client = self
                .get_rpc_client(app.clone())
                .ok_or(RemoteClientError::ClientNotFound)?;
            let support = self.capabilities.lock().unwrap().save;
            let result = compatible_rpc_call(
                support,
                9,
                client.save_network_instance_config(
                    BaseController::default(),
                    SaveNetworkInstanceConfigRequest {
                        inst_id: Some(id.into()),
                        config: Some(config.clone()),
                        source: config_source_to_rpc(source),
                    },
                ),
            )
            .await?;
            self.capabilities.lock().unwrap().save = Some(result.is_some());
            if result.is_some() {
                self.storage.fallback_configs.remove(&id);
            } else {
                self.storage.fallback_configs.insert(id);
            }
            self.storage
                .save_config(
                    &app,
                    id,
                    config,
                    PersistedConfigSource::from_runtime_source(source),
                )
                .map_err(RemoteClientError::PersistentError)?;
            Ok(())
        }

        async fn handle_remove_network_instances(
            &self,
            app: AppHandle,
            ids: Vec<Uuid>,
        ) -> Result<(), RemoteClientError<anyhow::Error>> {
            if ids.is_empty() {
                return Ok(());
            }
            let mut client = self
                .get_rpc_client(app.clone())
                .ok_or(RemoteClientError::ClientNotFound)?;
            let result = self.remove_via_rpc(client.as_mut(), &ids).await;
            if !self.local {
                let emitted = self
                    .storage
                    .save_configs(&app)
                    .and_then(|_| self.storage.save_enabled_networks(&app));
                result?;
                emitted.map_err(RemoteClientError::PersistentError)?;
                self.notify_vpn_stop_if_no_tun(&app)
                    .map_err(RemoteClientError::Other)?;
            } else {
                result?;
            }
            Ok(())
        }

        fn get_config_rpc_client(
            &self,
            _: AppHandle,
        ) -> Option<Box<dyn ConfigRpc<Controller = BaseController> + Send>> {
            Some(
                self.rpc_manager
                    .rpc_client()
                    .scoped_client::<ConfigRpcClientFactory<BaseController>>(1, 1, String::new()),
            )
        }

        fn get_rpc_client(
            &self,
            _: AppHandle,
        ) -> Option<Box<dyn WebClientService<Controller = BaseController> + Send>> {
            Some(
                self.rpc_manager
                    .rpc_client()
                    .scoped_client::<WebClientServiceClientFactory<BaseController>>(
                        1,
                        1,
                        "".to_string(),
                    ),
            )
        }

        fn get_storage(&self) -> &impl Storage<AppHandle, GUIConfig, anyhow::Error> {
            self.storage.as_ref()
        }

        async fn handle_get_network_config_with_source(
            &self,
            app: AppHandle,
            id: Uuid,
        ) -> Result<(NetworkConfig, ConfigSource), RemoteClientError<anyhow::Error>> {
            // Android's local adapter needs active or hook-prepared configs even
            // when RPC deliberately refuses read-only configuration exports.
            #[cfg(target_os = "android")]
            if self.local {
                let running = INSTANCE_MANAGER
                    .read()
                    .await
                    .as_ref()
                    .and_then(|instances| instances.config(id));
                let prepared = self
                    .storage
                    .network_configs
                    .get(&id)
                    .map(|entry| (entry.config.clone(), entry.source.to_runtime_source()));
                if let Some(config) = local_vpn_config(self.local, running, prepared)
                    .map_err(RemoteClientError::PersistentError)?
                {
                    return Ok(config);
                }
            }
            if self.storage.fallback_configs.contains(&id) {
                let ids = self.handle_list_network_instance_ids(app.clone()).await?;
                if !ids.running_inst_ids.contains(&id.into()) {
                    let cached = self
                        .storage
                        .network_configs
                        .get(&id)
                        .ok_or_else(|| RemoteClientError::NotFound(id.to_string()))?;
                    return Ok((cached.config.clone(), cached.source.to_runtime_source()));
                }
            }
            let client = self
                .get_rpc_client(app)
                .ok_or(RemoteClientError::ClientNotFound)?;
            let response = client
                .get_network_instance_config(
                    BaseController::default(),
                    GetNetworkInstanceConfigRequest {
                        inst_id: Some(id.into()),
                    },
                )
                .await?;
            let config = response
                .config
                .ok_or_else(|| RemoteClientError::NotFound(id.to_string()))?;
            let source = easytier_core::management::config_source_from_rpc(response.source)
                .or_else(|| {
                    self.storage
                        .network_configs
                        .get(&id)
                        .map(|entry| entry.source.to_runtime_source())
                })
                .unwrap_or(ConfigSource::User);
            Ok((config, source))
        }

        /// Core-managed state changes preserve its file and use server hooks.
        /// Only older cores need GUI-held configs and the legacy Run/Delete pair.
        async fn handle_update_network_state(
            &self,
            identify: AppHandle,
            inst_id: uuid::Uuid,
            disabled: bool,
        ) -> Result<(), RemoteClientError<anyhow::Error>> {
            let client = self
                .get_rpc_client(identify.clone())
                .ok_or(RemoteClientError::ClientNotFound)?;
            let support = self.capabilities.lock().unwrap().disable;
            let result = compatible_rpc_call(
                support,
                10,
                client.set_network_instance_enabled(
                    BaseController::default(),
                    SetNetworkInstanceEnabledRequest {
                        inst_id: Some(inst_id.into()),
                        enabled: !disabled,

                        ..Default::default()
                    },
                ),
            )
            .await?;
            self.capabilities.lock().unwrap().disable = Some(result.is_some());
            if result.is_none() {
                let (cfg, source) = self
                    .handle_get_network_config_with_source(identify.clone(), inst_id)
                    .await?;
                if disabled {
                    self.storage.fallback_configs.insert(inst_id);
                    self.storage
                        .save_config(
                            &identify,
                            inst_id,
                            cfg.clone(),
                            PersistedConfigSource::from_runtime_source(source),
                        )
                        .map_err(RemoteClientError::PersistentError)?;
                    let response = client
                        .delete_network_instance(
                            BaseController::default(),
                            DeleteNetworkInstanceRequest {
                                inst_ids: vec![inst_id.into()],

                                ..Default::default()
                            },
                        )
                        .await?;
                    ensure_removed(&[inst_id], &response.remain_inst_ids)?;
                } else {
                    client
                        .run_network_instance(
                            BaseController::default(),
                            RunNetworkInstanceRequest {
                                inst_id: Some(inst_id.into()),
                                config: Some(cfg.clone()),
                                overwrite: true,
                                source: config_source_to_rpc(source),
                            },
                        )
                        .await?;
                }
                self.storage
                    .save_config(
                        &identify,
                        inst_id,
                        cfg,
                        PersistedConfigSource::from_runtime_source(source),
                    )
                    .map_err(RemoteClientError::PersistentError)?;
            }

            if !self.local {
                self.storage
                    .update_network_config_state(identify.clone(), inst_id, disabled)
                    .await
                    .map_err(RemoteClientError::PersistentError)?;
                if disabled {
                    self.notify_vpn_stop_if_no_tun(&identify)
                        .map_err(RemoteClientError::Other)?;
                }
            }

            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use easytier::proto::api::manage::{NetworkConfig, WebClientServiceDescriptor};
        use easytier::proto::rpc_types::{
            __rt::{self, RpcClientFactory},
            descriptor::{MethodDescriptor, ServiceDescriptor},
            handler::Handler,
        };

        #[derive(Clone, Default)]
        struct LegacyWireService {
            running: Arc<std::sync::Mutex<std::collections::HashMap<Uuid, NetworkConfig>>>,
            protected: Arc<std::sync::Mutex<std::collections::HashSet<Uuid>>>,
            remove_requests: Arc<std::sync::Mutex<Vec<Vec<Uuid>>>>,
        }

        #[async_trait]
        impl Handler for LegacyWireService {
            type Descriptor = WebClientServiceDescriptor;
            type Controller = BaseController;

            async fn call(
                &self,
                _: BaseController,
                method: <Self::Descriptor as ServiceDescriptor>::Method,
                input: bytes::Bytes,
            ) -> Result<bytes::Bytes, RpcError> {
                use easytier::proto::api::manage::*;
                let index = method.index();
                if index == 11 {
                    let request: RemoveNetworkInstanceConfigRequest = __rt::decode(input.clone())?;
                    self.remove_requests
                        .lock()
                        .unwrap()
                        .push(request.inst_ids.into_iter().map(Into::into).collect());
                }
                if index > 8 {
                    let error = RpcError::InvalidMethodIndex(index, "WebClientService".to_string());
                    let encoded = __rt::encode(easytier::proto::error::Error::from(&error))?;
                    let decoded: easytier::proto::error::Error = __rt::decode(encoded)?;
                    return Err(RpcError::from(&decoded));
                }
                let mut running = self.running.lock().unwrap();
                match index {
                    2 => {
                        let request: RunNetworkInstanceRequest = __rt::decode(input)?;
                        let config = request.config.unwrap();
                        let id = request
                            .inst_id
                            .map(Uuid::from)
                            .unwrap_or_else(|| config.instance_id().parse().unwrap());
                        running.insert(id, config);
                        __rt::encode(RunNetworkInstanceResponse {
                            inst_id: Some(id.into()),
                        })
                    }
                    5 => __rt::encode(ListNetworkInstanceResponse {
                        inst_ids: running.keys().copied().map(Into::into).collect(),
                        disabled_inst_ids: Vec::new(),
                        supports_persisted_config_management: None,

                        ..Default::default()
                    }),
                    6 => {
                        let request: DeleteNetworkInstanceRequest = __rt::decode(input)?;
                        for id in request.inst_ids {
                            let id: Uuid = id.into();
                            if !self.protected.lock().unwrap().contains(&id) {
                                running.remove(&id);
                            }
                        }
                        __rt::encode(DeleteNetworkInstanceResponse {
                            remain_inst_ids: running.keys().copied().map(Into::into).collect(),
                        })
                    }
                    7 => {
                        let request: GetNetworkInstanceConfigRequest = __rt::decode(input)?;
                        __rt::encode(GetNetworkInstanceConfigResponse {
                            config: running.get(&request.inst_id.unwrap().into()).cloned(),
                            source: 0,
                        })
                    }
                    _ => Err(anyhow::anyhow!("unused legacy method").into()),
                }
            }
        }

        #[tokio::test]
        async fn gui_delete_clears_only_successful_fallbacks_and_never_bypasses_rejected_ids() {
            let service = LegacyWireService::default();
            let removed = Uuid::new_v4();
            let protected = Uuid::new_v4();
            service.protected.lock().unwrap().insert(protected);
            let storage = Arc::new(GUIStorage::new(Some("tcp://127.0.0.1:15999".to_string())));
            for id in [removed, protected] {
                let config = NetworkConfig {
                    instance_id: Some(id.to_string()),
                    ..Default::default()
                };
                service.running.lock().unwrap().insert(id, config.clone());
                storage.network_configs.insert(
                    id,
                    GUIConfig::new(id.to_string(), config, PersistedConfigSource::User),
                );
                storage.fallback_configs.insert(id);
                storage.enabled_networks.insert(id);
            }
            let manager = GUIClientManager {
                storage,
                rpc_manager: BidirectRpcManager::new(),
                capabilities: std::sync::Mutex::new(RuntimeCapabilities::default()),
                local: false,
            };
            let mut client = WebClientServiceClientFactory::<BaseController>::new(service.clone());
            assert!(
                manager
                    .remove_via_rpc(&mut client, &[removed, protected])
                    .await
                    .is_err()
            );
            assert!(!manager.storage.network_configs.contains_key(&removed));
            assert!(!manager.storage.fallback_configs.contains(&removed));
            assert!(manager.storage.network_configs.contains_key(&protected));
            assert!(manager.storage.enabled_networks.contains(&protected));
            assert_eq!(
                *service.remove_requests.lock().unwrap(),
                vec![vec![removed]]
            );
            assert!(
                manager
                    .remove_via_rpc(&mut client, &[protected])
                    .await
                    .is_err()
            );
            assert_eq!(service.remove_requests.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn official_eight_method_wire_protocol_supports_the_gui_fallback_lifecycle() {
            let client =
                WebClientServiceClientFactory::<BaseController>::new(LegacyWireService::default());
            let id = Uuid::new_v4();
            let config = NetworkConfig {
                instance_id: Some(id.to_string()),
                network_name: Some("draft".to_string()),
                ..Default::default()
            };
            let snapshot = client
                .list_network_instance(BaseController::default(), Default::default())
                .await
                .unwrap();
            assert_eq!(snapshot.supports_persisted_config_management, None);
            assert!(
                compatible_rpc_call(
                    None,
                    9,
                    client.save_network_instance_config(
                        BaseController::default(),
                        SaveNetworkInstanceConfigRequest {
                            inst_id: Some(id.into()),
                            config: Some(config.clone()),
                            source: 1,
                        }
                    )
                )
                .await
                .unwrap()
                .is_none()
            );
            assert!(
                client
                    .list_network_instance(BaseController::default(), Default::default())
                    .await
                    .unwrap()
                    .inst_ids
                    .is_empty()
            );
            client
                .run_network_instance(
                    BaseController::default(),
                    RunNetworkInstanceRequest {
                        inst_id: Some(id.into()),
                        config: Some(config.clone()),
                        overwrite: true,
                        source: 1,
                    },
                )
                .await
                .unwrap();
            let fetched = client
                .get_network_instance_config(
                    BaseController::default(),
                    GetNetworkInstanceConfigRequest {
                        inst_id: Some(id.into()),
                    },
                )
                .await
                .unwrap();
            assert_eq!(fetched.config.unwrap(), config);
            assert!(
                compatible_rpc_call(
                    None,
                    10,
                    client.set_network_instance_enabled(
                        BaseController::default(),
                        SetNetworkInstanceEnabledRequest {
                            inst_id: Some(id.into()),
                            enabled: false,
                            ..Default::default()
                        }
                    )
                )
                .await
                .unwrap()
                .is_none()
            );
            let deleted = client
                .delete_network_instance(
                    BaseController::default(),
                    DeleteNetworkInstanceRequest {
                        inst_ids: vec![id.into()],

                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            ensure_removed(&[id], &deleted.remain_inst_ids).unwrap();
            assert!(
                compatible_rpc_call(
                    None,
                    11,
                    client.remove_network_instance_config(
                        BaseController::default(),
                        RemoveNetworkInstanceConfigRequest {
                            inst_ids: vec![id.into()],
                            ..Default::default()
                        }
                    )
                )
                .await
                .unwrap()
                .is_none()
            );
            assert!(
                client
                    .list_network_instance(BaseController::default(), Default::default())
                    .await
                    .unwrap()
                    .inst_ids
                    .is_empty()
            );
            client
                .run_network_instance(
                    BaseController::default(),
                    RunNetworkInstanceRequest {
                        inst_id: Some(id.into()),
                        config: Some(config),
                        overwrite: true,
                        source: 1,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                client
                    .list_network_instance(BaseController::default(), Default::default())
                    .await
                    .unwrap()
                    .inst_ids,
                vec![id.into()]
            );
        }

        #[tokio::test]
        async fn legacy_fallback_requires_the_exact_unsupported_method() {
            let legacy = RpcError::InvalidMethodIndex(9, "WebClientService".to_string());
            assert_eq!(
                compatible_rpc_call::<()>(None, 9, async { Err(legacy) })
                    .await
                    .unwrap(),
                None
            );
            for error in [
                RpcError::ExecutionError(anyhow::anyhow!("permission denied")),
                RpcError::ExecutionError(anyhow::anyhow!("Timeout")),
                RpcError::InvalidMethodIndex(10, "WebClientService".to_string()),
                RpcError::InvalidMethodIndex(9, "OtherService".to_string()),
                RpcError::Shutdown,
            ] {
                assert!(
                    compatible_rpc_call::<()>(None, 9, async { Err(error) })
                        .await
                        .is_err()
                );
            }
        }

        #[tokio::test]
        async fn unsupported_service_names_are_decoded_after_the_proto_error_roundtrip() {
            for method in [9, 10, 11] {
                let error = RpcError::InvalidMethodIndex(method, "WebClientService".to_string());
                let encoded = __rt::encode(easytier::proto::error::Error::from(&error)).unwrap();
                let decoded: easytier::proto::error::Error = __rt::decode(encoded).unwrap();
                let rpc_error = RpcError::from(&decoded);
                assert!(unsupported_method(&rpc_error, method));
                assert!(
                    compatible_rpc_call::<()>(None, method, async { Err(rpc_error) })
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            assert!(!unsupported_method(
                &RpcError::InvalidMethodIndex(9, "\"OtherService\"".to_string()),
                9
            ));
            assert!(!unsupported_method(
                &RpcError::InvalidMethodIndex(9, "not-json-WebClientService".to_string()),
                9
            ));
        }

        #[tokio::test]
        async fn unknown_old_custom_core_keeps_its_extensions() {
            let capabilities = RuntimeCapabilities::default();
            assert_eq!(capabilities.save, None);
            assert_eq!(
                compatible_rpc_call(capabilities.save, 9, async { Ok(42) })
                    .await
                    .unwrap(),
                Some(42)
            );
            let mut capabilities = capabilities;
            capabilities.observe(Some(true));
            capabilities.observe(None);
            assert_eq!(capabilities.advertised, Some(true));
            assert_eq!(capabilities.disable, Some(true));
        }

        #[tokio::test]
        async fn known_legacy_does_not_send_the_extension_again() {
            let result =
                compatible_rpc_call::<()>(Some(false), 9, async { panic!("unexpected RPC") })
                    .await
                    .unwrap();
            assert_eq!(result, None);
        }

        #[test]
        fn a_successful_rpc_with_remaining_requested_ids_is_not_a_delete() {
            let requested = Uuid::new_v4();
            let unrelated = Uuid::new_v4();
            assert!(ensure_removed(&[requested], &[requested.into(), unrelated.into()]).is_err());
            assert!(ensure_removed(&[requested], &[unrelated.into()]).is_ok());
            assert_eq!(
                partition_removed(&[requested, unrelated], &[requested.into()]),
                (vec![unrelated], vec![requested])
            );
        }

        #[test]
        fn local_migration_restores_enabled_ids_but_remote_migration_never_replays_them() {
            let id = Uuid::new_v4();
            assert_eq!(
                migration_enabled_ids(true, vec![id.to_string(), "invalid".to_string()]),
                vec![id]
            );
            assert!(migration_enabled_ids(false, vec![id.to_string()]).is_empty());
        }

        #[test]
        fn local_android_vpn_can_read_the_complete_active_config_without_weakening_remote_reads() {
            let config = TomlConfigLoader::default();
            let mut flags = config.get_flags();
            flags.no_tun = false;
            flags.accept_dns = true;
            config.set_flags(flags);
            config.set_routes(Some(vec!["10.23.0.0/16".parse().unwrap()]));
            config.set_network_config_source(Some(ConfigSource::Web));
            let expected = NetworkConfig::new_from_config(&config).unwrap();
            let (loaded, source) = local_vpn_config(true, Some(config.clone()), None)
                .unwrap()
                .unwrap();
            assert_eq!(loaded, expected);
            assert_eq!(loaded.enable_magic_dns, Some(true));
            assert_eq!(loaded.no_tun, Some(false));
            assert_eq!(loaded.routes, vec!["10.23.0.0/16"]);
            assert_eq!(source, ConfigSource::Web);
            let prepared = Some((loaded.clone(), source));
            assert_eq!(
                local_vpn_config(true, None, prepared.clone()).unwrap(),
                prepared
            );
            assert!(
                local_vpn_config(false, Some(config), prepared)
                    .unwrap()
                    .is_none()
            );
            assert!(local_vpn_config(true, None, None).unwrap().is_none());
        }

        #[test]
        fn a_retired_local_backend_does_not_keep_stale_tun_or_fallback_state() {
            let storage = GUIStorage::new(None);
            let id = Uuid::new_v4();
            storage.network_configs.insert(
                id,
                GUIConfig::new(
                    id.to_string(),
                    NetworkConfig::default(),
                    PersistedConfigSource::User,
                ),
            );
            storage.enabled_networks.insert(id);
            storage.fallback_configs.insert(id);
            storage.clear();
            assert!(storage.network_configs.is_empty());
            assert!(storage.enabled_networks.is_empty());
            assert!(storage.fallback_configs.is_empty());
        }

        #[test]
        fn android_start_plans_are_pure_and_preserve_user_tun_ownership() {
            let id = Uuid::new_v4();
            let user = Uuid::new_v4();
            let web = Uuid::new_v4();
            let active = [
                ActiveInstanceForStart {
                    instance_id: user,
                    no_tun: false,
                    source: ConfigSource::User,
                },
                ActiveInstanceForStart {
                    instance_id: web,
                    no_tun: false,
                    source: ConfigSource::Web,
                },
            ];
            assert_eq!(
                tun_instances_to_stop(id, true, ConfigSource::User, &active).unwrap(),
                Vec::<Uuid>::new()
            );
            assert_eq!(
                tun_instances_to_stop(id, false, ConfigSource::User, &active).unwrap(),
                vec![user, web]
            );
            assert!(tun_instances_to_stop(id, false, ConfigSource::Web, &active).is_err());
            assert!(tun_instances_to_stop(user, false, ConfigSource::Web, &active).is_err());
            assert_eq!(
                tun_instances_to_stop(web, false, ConfigSource::Web, &active[1..]).unwrap(),
                Vec::<Uuid>::new()
            );
            assert_eq!(
                tun_instances_to_stop(id, false, ConfigSource::Web, &active[1..]).unwrap(),
                vec![web]
            );
        }

        #[test]
        fn stored_gui_config_defaults_missing_source_to_legacy() {
            let stored: StoredGuiConfig = serde_json::from_value(serde_json::json!({
                "config": NetworkConfig::default(),
            }))
            .unwrap();
            assert_eq!(stored.source, PersistedConfigSource::Legacy);
        }

        #[test]
        fn stored_gui_config_deserializes_webhook_source_as_web() {
            let stored: StoredGuiConfig = serde_json::from_value(serde_json::json!({
                "config": NetworkConfig::default(),
                "source": "webhook",
            }))
            .unwrap();
            assert_eq!(stored.source, PersistedConfigSource::Web);
        }

        #[test]
        fn stored_gui_config_defaults_unknown_source_to_legacy() {
            let stored: StoredGuiConfig = serde_json::from_value(serde_json::json!({
                "config": NetworkConfig::default(),
                "source": "unknown",
            }))
            .unwrap();
            assert_eq!(stored.source, PersistedConfigSource::Legacy);
        }

        #[test]
        fn persisted_source_merge_keeps_legacy_and_web_over_ambiguous_user() {
            assert_eq!(
                PersistedConfigSource::Legacy.merge_persisted(PersistedConfigSource::User),
                PersistedConfigSource::Legacy
            );
            assert_eq!(
                PersistedConfigSource::Web.merge_persisted(PersistedConfigSource::User),
                PersistedConfigSource::Web
            );
            assert_eq!(
                PersistedConfigSource::Legacy.merge_persisted(PersistedConfigSource::Web),
                PersistedConfigSource::Web
            );
        }

        #[test]
        fn only_web_configs_are_web_like() {
            assert!(!PersistedConfigSource::Legacy.is_web_like());
            assert!(!PersistedConfigSource::User.is_web_like());
            assert!(PersistedConfigSource::Web.is_web_like());
        }
    }
}

#[cfg(not(target_os = "android"))]
mod service {
    use anyhow::Context;

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    pub struct ServiceOptions {
        pub(super) config_dir: String,
        pub(super) rpc_portal: String,
        pub(super) file_log_level: String,
        pub(super) file_log_dir: String,
        pub(super) config_server: Option<String>,
        #[serde(default)]
        pub(super) machine_id: Option<String>,
    }
    impl ServiceOptions {
        fn to_args_vec(&self) -> Vec<std::ffi::OsString> {
            let mut args = vec![
                "--config-dir".into(),
                self.config_dir.clone().into(),
                "--rpc-portal".into(),
                self.rpc_portal.clone().into(),
                "--file-log-level".into(),
                self.file_log_level.clone().into(),
                "--file-log-dir".into(),
                self.file_log_dir.clone().into(),
                "--daemon".into(),
            ];

            if let Some(config_server) = &self.config_server {
                args.push("--config-server".into());
                args.push(config_server.clone().into());
            }

            if let Some(machine_id) = &self.machine_id {
                args.push("--machine-id".into());
                args.push(machine_id.clone().into());
            }

            args
        }
    }

    #[cfg(target_os = "macos")]
    fn service_environment() -> Option<Vec<(String, String)>> {
        // System LaunchDaemons run as root but launchd does not provide HOME.
        Some(vec![("HOME".to_string(), "/var/root".to_string())])
    }

    #[cfg(not(target_os = "macos"))]
    fn service_environment() -> Option<Vec<(String, String)>> {
        None
    }

    pub fn install(opts: ServiceOptions) -> anyhow::Result<()> {
        let service = easytier::service_manager::Service::new(env!("CARGO_PKG_NAME").to_string())?;
        let options = easytier::service_manager::ServiceInstallOptions {
            program: super::get_exe_path().into(),
            args: opts.to_args_vec(),
            work_directory: std::env::current_dir()?,
            environment: service_environment(),
            disable_autostart: false,
            description: Some("EasyTier Gui Service".to_string()),
            display_name: Some("EasyTier Gui Service".to_string()),
            disable_restart_on_failure: false,
        };
        service
            .install(&options)
            .with_context(|| "Failed to install service")?;
        Ok(())
    }

    pub fn uninstall() -> anyhow::Result<()> {
        let service = easytier::service_manager::Service::new(env!("CARGO_PKG_NAME").to_string())?;
        service.uninstall()?;
        Ok(())
    }

    pub fn set_status(enable: bool) -> anyhow::Result<()> {
        use easytier::service_manager::*;
        let service = Service::new(env!("CARGO_PKG_NAME").to_string())?;
        let status = service.status()?;
        if enable && status != ServiceStatus::Running {
            service.start().with_context(|| "Failed to start service")?;
        } else if !enable && status == ServiceStatus::Running {
            service.stop().with_context(|| "Failed to stop service")?;
        } else if status == ServiceStatus::NotInstalled {
            return Err(anyhow::anyhow!("Service not installed"));
        }
        Ok(())
    }

    pub fn status() -> anyhow::Result<easytier::service_manager::ServiceStatus> {
        let service = easytier::service_manager::Service::new(env!("CARGO_PKG_NAME").to_string())?;
        service.status()
    }

    #[cfg(test)]
    mod tests {
        use super::ServiceOptions;

        #[test]
        fn service_environment_matches_platform() {
            #[cfg(target_os = "macos")]
            assert_eq!(
                super::service_environment(),
                Some(vec![("HOME".to_string(), "/var/root".to_string())])
            );

            #[cfg(not(target_os = "macos"))]
            assert_eq!(super::service_environment(), None);
        }

        #[test]
        fn service_uses_the_gui_machine_id_and_selected_config_dir() {
            let opts = ServiceOptions {
                config_dir: "C:/shared/config.d".to_string(),
                rpc_portal: "127.0.0.1:15999".to_string(),
                file_log_level: "off".to_string(),
                file_log_dir: "C:/shared/logs".to_string(),
                config_server: None,
                machine_id: Some("33333333-3333-3333-3333-333333333333".to_string()),
            };
            let args = opts.to_args_vec();
            let args = args
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>();
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--config-dir", "C:/shared/config.d"])
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--machine-id", "33333333-3333-3333-3333-333333333333"])
            );
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run_gui() -> std::process::ExitCode {
    #[cfg(not(target_os = "android"))]
    if !check_sudo() {
        use std::process;
        process::exit(0);
    }

    setup_panic_handler();

    let mut builder = tauri::Builder::default();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            app.webview_windows()
                .values()
                .next()
                .expect("Sorry, no window found")
                .set_focus()
                .expect("Can't Bring Window to Focus");
        }));
    }

    builder = builder
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_vpnservice::init());

    let app = builder
        .setup(|app| {
            // for logging config
            let Ok(log_dir) = get_log_dir(app.app_handle()) else {
                return Ok(());
            };
            let config = LoggingConfig::builder()
                .file_logger(FileLoggerConfig {
                    dir: Some(log_dir.to_string_lossy().to_string()),
                    level: None,
                    file: None,
                    size_mb: None,
                    count: None,
                })
                .build();
            let Ok(_) = log::init(&config, true) else {
                return Ok(());
            };

            // for tray icon, menu need to be built in js
            #[cfg(not(target_os = "android"))]
            let _tray_menu = TrayIconBuilder::with_id("main")
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        toggle_window_visibility(app);
                    }
                })
                .icon(tauri::image::Image::from_bytes(include_bytes!(
                    "../icons/icon.png"
                ))?)
                .icon_as_template(true)
                .build(app)?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            persisted_configs::observe_local_configs,
            persisted_configs::patch_local_config,
            persisted_configs::set_local_config_enabled,
            persisted_configs::remove_local_config,
            parse_network_config,
            generate_network_config,
            run_network_instance,
            collect_network_info,
            get_vpn_portal_info,
            patch_vpn_portal_clients,
            set_logging_level,
            set_tun_fd,
            easytier_version,
            set_dock_visibility,
            list_network_instance_ids,
            remove_network_instance,
            update_network_config_state,
            save_network_config,
            validate_config,
            get_config,
            load_configs,
            get_network_metas,
            init_service,
            resolve_shared_config_dir,
            stop_local_backend,
            set_service_status,
            retire_conflicting_services,
            get_service_status,
            init_rpc_connection,
            is_client_running,
            init_web_client,
            is_web_client_connected,
            get_log_dir_path,
        ])
        .on_window_event(|_win, event| match event {
            #[cfg(not(target_os = "android"))]
            tauri::WindowEvent::CloseRequested { api, .. } => {
                let _ = _win.hide();
                let _ = set_dock_visibility(_win.app_handle().clone(), false);
                api.prevent_close();
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .unwrap();

    app.run(|_app, _event| {});

    std::process::ExitCode::SUCCESS
}

pub fn run_cli() -> std::process::ExitCode {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { easytier::core::main().await })
}
