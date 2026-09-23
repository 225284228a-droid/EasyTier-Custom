#![allow(dead_code)]

#[macro_use]
extern crate rust_i18n;

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use easytier::tunnel::websocket::WsTunnelListener;
use easytier::{
    common::{
        config::{ConsoleLoggerConfig, FileLoggerConfig, LoggingConfigLoader},
        constants::EASYTIER_VERSION,
        log,
        network::{local_ipv4, local_ipv6},
    },
    proto::rpc::standalone::{runtime_rpc_listener, runtime_udp_tunnel_listener},
    utils::panic::setup_panic_handler,
};
use easytier_core::{socket::SocketListener, tunnel::Tunnel};

use easytier::tunnel::IpScheme;
use mimalloc::MiMalloc;

mod client_manager;
mod db;
mod migrator;
mod restful;
mod webhook;

#[cfg(feature = "embed")]
mod web;

#[global_allocator]
static GLOBAL_MIMALLOC: MiMalloc = MiMalloc;

rust_i18n::i18n!("locales", fallback = "en");

#[derive(Parser, Debug)]
#[command(name = "easytier-web", author, version = EASYTIER_VERSION , about, long_about = None)]
struct Cli {
    #[arg(
        short,
        long,
        env = "ET_WEB_DB",
        default_value = "et.db",
        help = t!("cli.db").to_string()
    )]
    db: String,

    #[arg(
        long,
        env = "ET_WEB_CONSOLE_LOG_LEVEL",
        help = t!("cli.console_log_level").to_string(),
    )]
    console_log_level: Option<String>,

    #[arg(
        long,
        env = "ET_WEB_FILE_LOG_LEVEL",
        help = t!("cli.file_log_level").to_string(),
    )]
    file_log_level: Option<String>,

    #[arg(
        long,
        env = "ET_WEB_FILE_LOG_DIR",
        help = t!("cli.file_log_dir").to_string(),
    )]
    file_log_dir: Option<String>,

    #[arg(
        long,
        short='c',
        env = "ET_CONFIG_SERVER_PORT",
        default_value = "22020",
        help = t!("cli.config_server_port").to_string(),
    )]
    config_server_port: u16,

    #[arg(
        long,
        short='p',
        env = "ET_CONFIG_SERVER_PROTOCOL",
        default_value = "udp",
        help = t!("cli.config_server_protocol").to_string(),
    )]
    config_server_protocol: String,

    #[arg(
        long,
        env = "ET_CONFIG_SERVER_LISTENERS",
        value_delimiter = ',',
        num_args = 1..,
        help = t!("cli.config_server_listeners").to_string(),
    )]
    config_server_listeners: Option<Vec<String>>,

    #[arg(
        long,
        short='a',
        env = "ET_API_SERVER_PORT",
        default_value = "11211",
        help = t!("cli.api_server_port").to_string(),
    )]
    api_server_port: u16,

    #[arg(
        long,
        env = "ET_API_SERVER_ADDR",
        default_value = "0.0.0.0",
        help = t!("cli.api_server_addr").to_string(),
    )]
    api_server_addr: IpAddr,

    #[arg(
        long,
        env = "ET_GEOIP_DB",
        help = t!("cli.geoip_db").to_string(),
    )]
    geoip_db: Option<String>,

    #[arg(
        long,
        env = "ET_HEARTBEAT_MIN_RESPONSE_MS",
        default_value = "3500",
        help = t!("cli.heartbeat_min_response_ms").to_string(),
    )]
    heartbeat_min_response_ms: u64,

    #[arg(
        long,
        env = "ET_HEARTBEAT_TIMEOUT_MS",
        default_value = "15000",
        help = t!("cli.heartbeat_timeout_ms").to_string(),
    )]
    heartbeat_timeout_ms: u64,

    #[cfg(feature = "embed")]
    #[arg(
        long,
        short='l',
        env = "ET_WEB_SERVER_PORT",
        help = t!("cli.web_server_port").to_string(),
    )]
    web_server_port: Option<u16>,

    #[cfg(feature = "embed")]
    #[arg(
        long,
        env = "ET_WEB_SERVER_ADDR",
        default_value = "0.0.0.0",
        help = t!("cli.web_server_addr").to_string(),
    )]
    web_server_addr: IpAddr,

    #[cfg(feature = "embed")]
    #[arg(
        long,
        env = "ET_NO_WEB",
        help = t!("cli.no_web").to_string(),
        default_value = "false"
    )]
    no_web: bool,

    #[cfg(feature = "embed")]
    #[arg(
        long,
        env = "ET_API_HOST",
        help = t!("cli.api_host").to_string()
    )]
    api_host: Option<url::Url>,

    #[command(flatten)]
    feature_flags: FeatureFlags,

    #[command(flatten)]
    oidc: restful::oidc::OidcOptions,

    #[command(flatten)]
    webhook: WebhookOptions,
}

#[derive(Debug, Clone, Default, clap::Args)]
pub struct WebhookOptions {
    /// Base URL of the webhook endpoint for token validation and event delivery.
    /// When set, incoming tokens are validated via this webhook before local fallback.
    #[arg(long, env = "ET_WEBHOOK_URL")]
    pub webhook_url: Option<String>,

    /// Shared secret used to authenticate outbound webhook calls.
    #[arg(long, env = "ET_WEBHOOK_SECRET", hide_env_values = true)]
    pub webhook_secret: Option<String>,

    /// Token for X-Internal-Auth header. When set, API requests with this header
    /// bypass session authentication.
    #[arg(long, env = "ET_INTERNAL_AUTH_TOKEN", hide_env_values = true)]
    pub internal_auth_token: Option<String>,

    /// Stable identifier for this easytier-web instance when routing webhook callbacks.
    #[arg(long, env = "ET_WEB_INSTANCE_ID")]
    pub web_instance_id: Option<String>,

    /// Reachable base URL for this easytier-web instance's internal REST API.
    #[arg(long, env = "ET_WEB_INSTANCE_API_BASE_URL")]
    pub web_instance_api_base_url: Option<String>,
}

#[derive(Debug, Clone, Default, clap::Args)]
pub struct FeatureFlags {
    /// Whether user registration via the web UI is disabled.
    #[arg(
        long,
        env = "ET_DISABLE_REGISTRATION",
        default_value = "false",
        help = t!("cli.disable_registration").to_string()
    )]
    pub disable_registration: bool,

    /// Whether to auto-create users when they connect via heartbeat with an unknown token.
    #[arg(
        long,
        env = "ET_ALLOW_AUTO_CREATE_USER",
        default_value = "false",
        help = t!("cli.allow_auto_create_user").to_string()
    )]
    pub allow_auto_create_user: bool,
}

impl LoggingConfigLoader for &Cli {
    fn get_console_logger_config(&self) -> ConsoleLoggerConfig {
        ConsoleLoggerConfig {
            level: self.console_log_level.clone(),
        }
    }

    fn get_file_logger_config(&self) -> FileLoggerConfig {
        FileLoggerConfig {
            dir: self.file_log_dir.clone(),
            level: self.file_log_level.clone(),
            file: None,
            size_mb: None,
            count: None,
        }
    }
}

pub const CONFIG_SERVER_DEFAULT_PORT: u16 = 22020;

/// Protocols accepted by `--config-server-listeners`.
const SUPPORTED_CONFIG_SERVER_PROTOCOLS: &[&str] = &["tcp", "udp", "ws", "wss"];

fn parse_config_server_listener_urls(entries: &[String]) -> anyhow::Result<Vec<url::Url>> {
    let mut urls = Vec::new();
    for entry in entries {
        for raw in entry.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let url = url::Url::parse(raw)
                .with_context(|| format!("invalid config server listener url {raw:?}"))?;
            if url.host_str().map(str::trim).unwrap_or("").is_empty() {
                anyhow::bail!("config server listener url {raw:?} must include a host");
            }
            if !SUPPORTED_CONFIG_SERVER_PROTOCOLS.contains(&url.scheme()) {
                anyhow::bail!(
                    "unsupported config server listener protocol {:?} in {raw:?}, supported protocols: {}",
                    url.scheme(),
                    SUPPORTED_CONFIG_SERVER_PROTOCOLS.join(", ")
                );
            }
            urls.push(url);
        }
    }
    Ok(urls)
}

fn is_wildcard_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_unspecified(),
        // tcp/udp are non-special schemes for the url crate, so wildcard ip
        // literals arrive as Domain instead of Ipv4/Ipv6 hosts.
        Some(url::Host::Domain(domain)) => matches!(domain, "0.0.0.0" | "::" | "[::]"),
        None => false,
    }
}

fn normalize_listener_url(url: &url::Url) -> url::Url {
    let mut normalized = url.clone();
    if normalized.port().is_none() {
        normalized
            .set_port(Some(CONFIG_SERVER_DEFAULT_PORT))
            .expect("listener url host is validated before normalization");
    }
    normalized
}

pub fn get_listener_by_url(
    scheme: IpScheme,
    l: &url::Url,
) -> Option<Box<dyn SocketListener<Accepted = Box<dyn Tunnel>>>> {
    Some(match scheme {
        IpScheme::Tcp => {
            let addr = l
                .socket_addrs(|| Some(CONFIG_SERVER_DEFAULT_PORT))
                .ok()?
                .into_iter()
                .next()?;
            Box::new(runtime_rpc_listener(addr))
        }
        IpScheme::Udp => {
            let addr = l
                .socket_addrs(|| Some(CONFIG_SERVER_DEFAULT_PORT))
                .ok()?
                .into_iter()
                .next()?;
            Box::new(runtime_udp_tunnel_listener(l.clone(), addr))
        }
        IpScheme::Ws | IpScheme::Wss => Box::new(WsTunnelListener::new(l.clone())),
        _ => return None,
    })
}

/// Build one listener entry per bind target. Wildcard tcp/udp urls are expanded
/// to dual-stack ([::] + 0.0.0.0) targets gated on local address availability.
async fn create_config_server_listeners(
    urls: &[url::Url],
) -> Vec<(
    url::Url,
    Box<dyn SocketListener<Accepted = Box<dyn Tunnel>>>,
)> {
    let mut listeners = Vec::new();
    for url in urls {
        let Ok(scheme) = url.scheme().parse::<IpScheme>() else {
            eprintln!(
                "Warning: skipping config server listener with unsupported protocol {:?}",
                url.scheme()
            );
            continue;
        };
        let url = normalize_listener_url(url);
        match scheme {
            IpScheme::Tcp | IpScheme::Udp => {
                let port = url.port().unwrap_or(CONFIG_SERVER_DEFAULT_PORT);
                let mut bind_targets: Vec<url::Url> = Vec::new();
                if is_wildcard_host(&url) {
                    if local_ipv6().await.is_ok() {
                        bind_targets.push(
                            format!("{scheme}://[::]:{port}")
                                .parse()
                                .expect("valid url"),
                        );
                    }
                    if local_ipv4().await.is_ok() {
                        bind_targets.push(
                            format!("{scheme}://0.0.0.0:{port}")
                                .parse()
                                .expect("valid url"),
                        );
                    }
                } else {
                    bind_targets.push(url.clone());
                }
                for target in bind_targets {
                    match get_listener_by_url(scheme, &target) {
                        Some(listener) => listeners.push((target, listener)),
                        None => {
                            eprintln!("Warning: failed to create {scheme} listener for {target}")
                        }
                    }
                }
            }
            IpScheme::Ws | IpScheme::Wss => {
                listeners.push((url.clone(), Box::new(WsTunnelListener::new(url.clone()))));
            }
            _ => {
                eprintln!(
                    "Warning: skipping config server listener with unsupported protocol {:?}",
                    url.scheme()
                );
            }
        }
    }
    listeners
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let locale = sys_locale::get_locale().unwrap_or_else(|| String::from("en-US"));
    rust_i18n::set_locale(&locale);
    setup_panic_handler();

    let cli = Cli::parse();
    log::init_with_default_console_targets(&cli, false, &["CORE", "easytier_web"]).unwrap();
    tracing::info!(
        version = EASYTIER_VERSION,
        web_instance_id = ?cli.webhook.web_instance_id,
        api_address = %cli.api_server_addr,
        api_port = cli.api_server_port,
        config_protocol = %cli.config_server_protocol,
        config_port = cli.config_server_port,
        heartbeat_min_response_ms = cli.heartbeat_min_response_ms,
        heartbeat_timeout_ms = cli.heartbeat_timeout_ms,
        webhook_enabled = cli.webhook.webhook_url.as_deref().is_some_and(|url| !url.trim().is_empty()),
        rust_log_override = std::env::var_os("RUST_LOG").is_some(),
        console_log_override = cli.console_log_level.is_some(),
        "easytier-web starting"
    );

    // Validate OIDC configuration: check split-deploy specific requirements
    // Basic OIDC parameter validation is handled in OidcConfig::from_params
    if cli.oidc.any_param_provided() {
        let is_split_deploy = {
            #[cfg(feature = "embed")]
            {
                let embed_split_by_port = cli.web_server_port.is_some()
                    && cli.web_server_port != Some(cli.api_server_port);
                cli.no_web || embed_split_by_port
            }
            #[cfg(not(feature = "embed"))]
            {
                true
            }
        };

        if is_split_deploy && cli.oidc.oidc_frontend_base_url.is_none() {
            eprintln!("Error: --oidc-frontend-base-url is required in split-deploy mode");
            eprintln!(
                "When frontend and API are deployed separately, you must specify the frontend URL"
            );
            eprintln!("Example: --oidc-frontend-base-url http://your-frontend-domain.com");
            std::process::exit(1);
        }
    }

    // let db = db::Db::new(":memory:").await.unwrap();
    let db = db::Db::new(cli.db).await.unwrap();
    let feature_flags = Arc::new(cli.feature_flags);
    let webhook_config = Arc::new(webhook::WebhookConfig::new(
        cli.webhook.webhook_url,
        cli.webhook.webhook_secret,
        cli.webhook.internal_auth_token,
        cli.webhook.web_instance_id,
        cli.webhook.web_instance_api_base_url,
    ));
    let listener_urls = match cli
        .config_server_listeners
        .as_ref()
        .filter(|entries| !entries.is_empty())
    {
        Some(entries) => match parse_config_server_listener_urls(entries) {
            Ok(urls) if !urls.is_empty() => urls,
            Ok(_) => {
                eprintln!(
                    "Error: --config-server-listeners is set but contains no valid listener url"
                );
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("Error: {error:#}");
                std::process::exit(1);
            }
        },
        None => {
            let protocol = cli.config_server_protocol.trim();
            if !SUPPORTED_CONFIG_SERVER_PROTOCOLS.contains(&protocol) {
                eprintln!(
                    "Error: unsupported config server protocol {protocol:?}, supported protocols: {}",
                    SUPPORTED_CONFIG_SERVER_PROTOCOLS.join(", ")
                );
                std::process::exit(1);
            }
            vec![
                format!("{protocol}://0.0.0.0:{}", cli.config_server_port)
                    .parse()
                    .expect("legacy config server listener url should be valid"),
            ]
        }
    };

    let heartbeat_policy = client_manager::HeartbeatPolicy::from_millis(
        cli.heartbeat_min_response_ms,
        cli.heartbeat_timeout_ms,
    )
    .unwrap_or_else(|error| {
        eprintln!("Invalid heartbeat configuration: {error}");
        std::process::exit(2);
    });
    let mut mgr = client_manager::ClientManager::new(
        db.clone(),
        cli.geoip_db,
        heartbeat_policy,
        feature_flags.clone(),
        webhook_config.clone(),
    );
    let mut bound_urls = Vec::new();
    for (url, listener) in create_config_server_listeners(&listener_urls).await {
        match mgr.add_listener(listener).await {
            Ok(local_url) => {
                tracing::info!(?local_url, "config server listener started");
                bound_urls.push(local_url);
            }
            Err(error) => {
                tracing::warn!(%url, %error, "failed to start config server listener");
            }
        }
    }
    if bound_urls.is_empty() {
        panic!("Failed to listen on any config server listener: {listener_urls:?}");
    }

    let mgr = Arc::new(mgr);

    #[cfg(feature = "embed")]
    let (web_router_restful, web_router_static) = if cli.no_web {
        (None, None)
    } else {
        let web_router = web::build_router(cli.api_host.clone());
        if cli.web_server_port.is_none()
            || (cli.web_server_port == Some(cli.api_server_port)
                && cli.web_server_addr == cli.api_server_addr)
        {
            (Some(web_router), None)
        } else {
            (None, Some(web_router))
        }
    };
    #[cfg(not(feature = "embed"))]
    let web_router_restful = None;

    let oidc_config = if cli.oidc.oidc_issuer_url.is_some() {
        match restful::oidc::OidcConfig::from_params(cli.oidc).await {
            Ok(config) => config,
            Err(e) => {
                eprintln!("Failed to initialize OIDC: {:?}", e);
                eprintln!("Please check your OIDC configuration (issuer URL, client ID, etc.)");
                std::process::exit(1);
            }
        }
    } else {
        restful::oidc::OidcConfig::disabled()
    };

    let _restful_server_tasks = restful::RestfulServer::new(
        std::net::SocketAddr::new(cli.api_server_addr, cli.api_server_port),
        mgr.clone(),
        db,
        web_router_restful,
        feature_flags,
        oidc_config,
        webhook_config,
    )
    .await
    .unwrap()
    .start()
    .await
    .unwrap();

    #[cfg(feature = "embed")]
    let _web_server_task = if let Some(web_router) = web_router_static {
        Some(
            web::WebServer::new(
                std::net::SocketAddr::new(cli.web_server_addr, cli.web_server_port.unwrap_or(0)),
                web_router,
            )
            .await
            .unwrap()
            .start()
            .await
            .unwrap(),
        )
    } else {
        None
    };

    tokio::signal::ctrl_c().await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_entries(entries: &[&str]) -> anyhow::Result<Vec<url::Url>> {
        parse_config_server_listener_urls(
            &entries
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<String>>(),
        )
    }

    #[test]
    fn parses_repeated_comma_separated_and_mixed_protocol_urls() {
        let urls = parse_entries(&[
            "udp://0.0.0.0:22020,tcp://0.0.0.0:22021",
            "  ws://[::]:22022  ",
            "",
        ])
        .unwrap();
        assert_eq!(urls.len(), 3);
        assert_eq!(urls[0].scheme(), "udp");
        assert_eq!(urls[0].port(), Some(22020));
        assert_eq!(urls[1].scheme(), "tcp");
        assert_eq!(urls[1].port(), Some(22021));
        assert_eq!(urls[2].scheme(), "ws");
        assert_eq!(urls[2].port(), Some(22022));
    }

    #[test]
    fn rejects_unsupported_protocol_and_missing_host() {
        assert!(parse_entries(&["quic://0.0.0.0:22020"]).is_err());
        assert!(parse_entries(&["udp://:22020"]).is_err());
        assert!(parse_entries(&["not a url"]).is_err());
        assert!(parse_entries(&["udp://0.0.0.0:22020", "tcp://bad url"]).is_err());
    }

    #[test]
    fn detects_wildcard_hosts_for_dual_stack_expansion() {
        let v4_wildcard = "udp://0.0.0.0:22020".parse::<url::Url>().unwrap();
        let v6_wildcard = "tcp://[::]:22020".parse::<url::Url>().unwrap();
        let specific_v4 = "tcp://192.168.1.10:22020".parse::<url::Url>().unwrap();
        let specific_v6 = "tcp://[::1]:22020".parse::<url::Url>().unwrap();
        assert!(is_wildcard_host(&v4_wildcard));
        assert!(is_wildcard_host(&v6_wildcard));
        assert!(!is_wildcard_host(&specific_v4));
        assert!(!is_wildcard_host(&specific_v6));
    }

    #[test]
    fn fills_default_port_when_missing() {
        let no_port = "udp://0.0.0.0".parse::<url::Url>().unwrap();
        assert_eq!(
            normalize_listener_url(&no_port).port(),
            Some(CONFIG_SERVER_DEFAULT_PORT)
        );
        let with_port = "wss://0.0.0.0:22023".parse::<url::Url>().unwrap();
        assert_eq!(normalize_listener_url(&with_port).port(), Some(22023));
    }
}
