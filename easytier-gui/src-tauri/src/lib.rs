// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod elevate;
mod local_backend;
mod service_conflicts;
mod shared_config;

use local_backend::{
    get_service_status, retire_conflicting_services, set_service_status, stop_local_backend,
    stop_local_backend_inner,
};

#[cfg(target_os = "linux")]
mod linux_tray;

use anyhow::Context;
use easytier::instance::factory::MobileTunConfig;
#[cfg(target_os = "android")]
use easytier::instance::factory::subscribe_native_instance_event;
#[cfg(any(target_os = "android", target_os = "ios"))]
use easytier::instance::factory::{
    attach_mobile_tun_fd_with_generation, get_mobile_tun_config as native_mobile_tun_config,
};
use easytier::proto::api::config::{
    ConfigPatchAction, ConfigRpc, ConfigRpcClientFactory, InstanceConfigPatch, PatchConfigRequest,
    VpnPortalClientPatch,
};
use easytier::proto::api::instance::{
    GetVpnPortalInfoRequest, GetVpnStatusRequest, GetVpnStatusResponse, InstanceIdentifier,
    PeerManageRpc, PeerManageRpcClientFactory, VpnPortalInfo, VpnPortalRpc,
    VpnPortalRpcClientFactory, instance_identifier,
};
use easytier::proto::api::manage::{
    CollectNetworkInfoResponse, ValidateConfigResponse, VpnPortalClientConfig, WebClientService,
    WebClientServiceClientFactory,
};
use easytier::proto::rpc_types::controller::BaseController;
use easytier::web_client::{self, WebClient};
use easytier::{
    common::config::{NetworkConfig, NetworkConfigExt},
    common::{
        MachineIdOptions,
        config::{ConfigLoader, FileLoggerConfig, LoggingConfig, TomlConfigLoader},
        log, resolve_machine_id,
    },
    instance::factory::{NativeInstanceManager, native_instance_manager_with_config_dir},
    proto::rpc::standalone::{runtime_rpc_dialer, runtime_rpc_listener},
    rpc_service::ApiRpcServer,
    utils::panic::setup_panic_handler,
};
use easytier_core::management::InstanceStateStore;
use easytier_core::{
    connectivity::protocol::raw::TunnelDialer as _, process_runtime::CoreProcessRuntime,
    socket::SocketListener, tunnel::Tunnel,
};
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard};
use uuid::Uuid;

use shared_config::{gui_machine_id, restore_managed_configs, shared_config_dir};
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
async fn collect_network_info(instance_id: String) -> Result<CollectNetworkInfoResponse, String> {
    let instance_id = instance_id
        .parse()
        .map_err(|e: uuid::Error| e.to_string())?;
    get_client_manager!()?
        .collect_network_info(instance_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn get_vpn_status(instance_id: String) -> Result<Option<GetVpnStatusResponse>, String> {
    let instance_id = instance_id
        .parse::<uuid::Uuid>()
        .map_err(|e| e.to_string())?;
    let client_manager = get_client_manager!()?;
    let client = client_manager
        .rpc_manager
        .rpc_client()
        .scoped_client::<PeerManageRpcClientFactory<BaseController>>(1, 1, "".to_string());
    let response = client
        .get_vpn_status(
            BaseController::default(),
            GetVpnStatusRequest {
                instance: Some(InstanceIdentifier {
                    selector: Some(instance_identifier::Selector::Id(instance_id.into())),
                }),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(response))
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
    // Core persists the patch into the instance's config file.
    client_manager
        .config_rpc()
        .patch_config(
            BaseController::default(),
            PatchConfigRequest {
                instance: Some(InstanceIdentifier {
                    selector: Some(instance_identifier::Selector::Id(instance_id.into())),
                }),
                patch: Some(InstanceConfigPatch {
                    vpn_portal_clients: vec![VpnPortalClientPatch {
                        action: action as i32,
                        client,
                    }],
                    ..Default::default()
                }),
            },
        )
        .await
        .map(|_| ())
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
async fn set_tun_fd(
    fd: i32,
    instance_ids: Option<Vec<Uuid>>,
    generation: Option<String>,
) -> Result<MobileTunConfig, String> {
    let Some(instance_manager) = INSTANCE_MANAGER.read().await.clone() else {
        return Err("set_tun_fd is not supported in remote mode".to_string());
    };
    let generation = generation
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("TUN generation must be an unsigned decimal integer".to_string());
            }
            value.parse::<u64>().map_err(|e| e.to_string())
        })
        .transpose()?;
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return attach_mobile_tun_fd_with_generation(
        instance_manager.as_ref(),
        fd,
        instance_ids.as_deref(),
        generation,
    )
    .await
    .map_err(|error| error.to_string());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = (instance_manager, fd, instance_ids, generation);
        Err("mobile TUN attachment is unavailable on this platform".to_owned())
    }
}

#[tauri::command]
async fn get_mobile_tun_config(instance_ids: Vec<Uuid>) -> Result<MobileTunConfig, String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let Some(manager) = INSTANCE_MANAGER.read().await.clone() else {
            return Err("mobile TUN configuration is not supported in remote mode".to_owned());
        };
        Ok(native_mobile_tun_config(manager.as_ref(), &instance_ids).await)
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = instance_ids;
        Err("mobile TUN configuration is unavailable on this platform".to_owned())
    }
}

#[tauri::command]
async fn list_network_instance_ids() -> Result<manager::ListNetworkInstanceIdsResponse, String> {
    get_client_manager!()?
        .list_network_instance_ids()
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn validate_config(config: NetworkConfig) -> Result<ValidateConfigResponse, String> {
    get_client_manager!()?
        .validate_config(config)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Android prepares its VPN before an instance starts, so a configuration
/// that is about to run is consulted before the running instance.
#[tauri::command]
async fn mobile_network_profile(
    instance_id: String,
) -> Result<manager::MobileNetworkProfile, String> {
    let instance_id: Uuid = instance_id
        .parse()
        .map_err(|e: uuid::Error| e.to_string())?;
    if let Some(profile) = manager::PREPARED_PROFILES.get(&instance_id) {
        return Ok(profile.clone());
    }
    let instances = INSTANCE_MANAGER
        .read()
        .await
        .clone()
        .ok_or("Network profiles are only known for the local Core")?;
    let config = instances
        .config(instance_id)
        .ok_or_else(|| format!("No configuration for network instance {instance_id}"))?;
    manager::MobileNetworkProfile::from_config(&config)
}

#[tauri::command]
async fn get_network_metas(
    instance_ids: Vec<uuid::Uuid>,
) -> Result<manager::GetNetworkMetasResponse, String> {
    get_client_manager!()?
        .get_network_metas(instance_ids)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[cfg(target_os = "android")]
#[tauri::command]
fn init_service() -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn init_service(app: AppHandle, opts: Option<service::ServiceOptions>) -> Result<(), String> {
    let _lifecycle = BACKEND_LIFECYCLE.lock().await;
    // SCM calls can block for seconds; run them on the blocking pool instead
    // of the async runtime / main thread.
    match opts {
        Some(mut args) => {
            stop_local_backend_inner().await?;
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

#[cfg(test)]
mod managed_restore_tests;

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
) -> Result<(), String> {
    let _lifecycle = BACKEND_LIFECYCLE.lock().await;
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if is_normal_mode {
        let status = tokio::task::spawn_blocking(service::status)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("{error:#}"))?;
        if status == easytier::service_manager::ServiceStatus::Running {
            return Err(
                "The GUI service is running; stop it before starting the local Core".into(),
            );
        }
    }
    let desired_config_dir = if is_normal_mode {
        Some(shared_config_dir(&app, config_dir.as_deref())?)
    } else {
        None
    };
    let current_config_dir = INSTANCE_CONFIG_DIR.read().await.clone();
    let previous_retired = INSTANCE_MANAGER
        .read()
        .await
        .as_ref()
        .is_some_and(|manager| manager.local_config_catalog().is_retired());
    if !is_normal_mode
        || previous_retired
        || (current_config_dir.is_some() && current_config_dir != desired_config_dir)
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
        manager::GUIClientManager::new(client_url, local_process_runtime),
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

mod manager;

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

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UserTrayUpdate {
    Menu { show: String, quit: String },
    RunState { running: bool },
    Tooltip { tooltip: String },
}

#[tauri::command]
fn update_user_tray(update: UserTrayUpdate) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        linux_tray::update(update).map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = update;
        Err("The tray is managed by this GUI process".to_string())
    }
}

#[tauri::command]
fn uses_user_tray() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_tray::is_controlled()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run_gui() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    if !elevate::Command::is_elevated() {
        return linux_tray::run();
    }

    #[cfg(not(target_os = "android"))]
    if !check_sudo() {
        use std::process;
        process::exit(0);
    }

    setup_panic_handler();
    easytier::utils::init_crypto_provider();

    let mut builder = tauri::Builder::default();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        // The user-session tray already enforces a single GUI instance.
        // Its elevated child must not compete for the same D-Bus name.
        if !uses_user_tray() {
            builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
                app.webview_windows()
                    .values()
                    .next()
                    .expect("Sorry, no window found")
                    .set_focus()
                    .expect("Can't Bring Window to Focus");
            }));
        }
    }

    builder = builder
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_vpnservice::init());

    let app = builder
        .setup(|app| {
            #[cfg(target_os = "linux")]
            if linux_tray::is_controlled() {
                linux_tray::listen(app.handle().clone())?;
            }

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
            if !uses_user_tray() {
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
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            persisted_configs::observe_local_configs,
            persisted_configs::patch_local_config,
            persisted_configs::update_network_config_state,
            persisted_configs::remove_network_instance,
            parse_network_config,
            generate_network_config,
            collect_network_info,
            get_vpn_status,
            get_vpn_portal_info,
            patch_vpn_portal_clients,
            set_logging_level,
            set_tun_fd,
            get_mobile_tun_config,
            easytier_version,
            set_dock_visibility,
            list_network_instance_ids,
            validate_config,
            mobile_network_profile,
            get_network_metas,
            init_service,
            shared_config::resolve_shared_config_dir,
            stop_local_backend,
            set_service_status,
            retire_conflicting_services,
            get_service_status,
            init_rpc_connection,
            is_client_running,
            init_web_client,
            is_web_client_connected,
            get_log_dir_path,
            uses_user_tray,
            update_user_tray,
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
