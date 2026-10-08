//! Opt-in interoperability against an independently built upstream CLI.
//!
//! Set OFFICIAL_EASYTIER_BIN to the upstream easytier-core executable, then run
//! `cargo test -p easytier --lib tunnel::websocket::official_interop -- --ignored`.
//! The process uses a temporary config directory, loopback listeners and no TUN.
//! Upstream builds need `management`, `websocket` and packet proxy support
//! (for example the standard `smoltcp` feature): upstream `no_tun` requires it.

use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, ensure};
use easytier_core::{
    config::toml::TomlConfig, process_runtime::CoreProcessRuntime,
    rpc::bidirect::BidirectRpcManager, socket::SocketListener,
};
use futures::{SinkExt, StreamExt};
use tokio::time::{Instant, sleep, timeout};
use tokio_websockets::{ClientBuilder, MaybeTlsStream, Message};

use crate::{
    instance::test_instance::TestInstance,
    proto::{
        api::manage::{
            ListNetworkInstanceRequest, WebClientService, WebClientServiceClientFactory,
        },
        rpc_types::{self, controller::BaseController},
        web::{
            GetFeatureRequest, GetFeatureResponse, HeartbeatRequest, HeartbeatResponse,
            WebServerService, WebServerServiceServer,
        },
    },
};

use super::{WsTunnelListener, get_insecure_tls_client_config, init_crypto_provider};

const DEADLINE: Duration = Duration::from_secs(30);

fn hide_process(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn official_binary() -> anyhow::Result<PathBuf> {
    let path = std::env::var_os("OFFICIAL_EASYTIER_BIN")
        .map(PathBuf::from)
        .context("set OFFICIAL_EASYTIER_BIN to an independently built upstream easytier-core")?;
    ensure!(
        path.is_file(),
        "official executable does not exist: {}",
        path.display()
    );
    let path = std::fs::canonicalize(path)?;
    let mut command = Command::new(&path);
    hide_process(&mut command);
    let version = command
        .arg("--version")
        .output()
        .context("read official CLI version")?;
    ensure!(version.status.success(), "official CLI --version failed");
    eprintln!(
        "official executable: {} ({})",
        path.display(),
        String::from_utf8_lossy(&version.stdout).trim()
    );
    Ok(path)
}

struct OfficialProcess {
    child: Child,
    log: PathBuf,
    _directory: tempfile::TempDir,
}

impl OfficialProcess {
    fn start(binary: &Path, config: Option<&str>, extra: &[String]) -> anyhow::Result<Self> {
        let directory =
            tempfile::tempdir().context("create isolated official process directory")?;
        let log = directory.path().join("official.log");
        let config_directory = directory.path().join("config.d");
        std::fs::create_dir(&config_directory)?;
        let output = File::create(&log)?;
        let mut command = Command::new(binary);
        hide_process(&mut command);
        // Do not inherit configuration overrides from the developer's shell.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ET_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(directory.path())
            .env("RUST_LOG", "warn")
            .arg("--config-dir")
            .arg(config_directory)
            .arg("--machine-id")
            .arg(uuid::Uuid::new_v4().to_string())
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output);
        if let Some(config) = config {
            let path = directory.path().join("network.toml");
            std::fs::write(&path, config)?;
            command.arg("--config-file").arg(path);
        }
        command.args(extra);
        let child = command
            .spawn()
            .context("launch isolated official process")?;
        Ok(Self {
            child,
            log,
            _directory: directory,
        })
    }

    fn logs(&self) -> String {
        let bytes = std::fs::read(&self.log).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        let tail = text.chars().rev().take(6000).collect::<String>();
        tail.chars().rev().collect()
    }

    fn ensure_running(&mut self) -> anyhow::Result<()> {
        if let Some(status) = self.child.try_wait()? {
            anyhow::bail!("official process exited ({status}):\n{}", self.logs());
        }
        Ok(())
    }

    async fn wait_listener(&mut self, port: u16) -> anyhow::Result<()> {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            self.ensure_running()?;
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("official listener did not bind: {}", self.logs());
    }
}

impl Drop for OfficialProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn network_config(name: &str, listener: Option<&str>, peer: Option<&str>) -> String {
    let listeners = listener.map(|url| format!("\"{url}\"")).unwrap_or_default();
    let peers = peer
        .map(|url| format!("\n[[peer]]\nuri = \"{url}\"\n"))
        .unwrap_or_default();
    format!(
        r#"hostname = "loopback-interop"
listeners = [{listeners}]
stun_servers = []
stun_servers_v6 = []
[network_identity]
network_name = "{name}"
network_secret = "loopback-interop-secret"
[flags]
no_tun = true
enable_ipv6 = false
enable_encryption = false
disable_p2p = true
disable_tcp_hole_punching = true
disable_udp_hole_punching = true
disable_upnp = true
bind_device = false
{peers}"#
    )
}

async fn peer_interop(scheme: &str, official_listens: bool, padding: &str) -> anyhow::Result<()> {
    let binary = official_binary()?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let plain_url = format!("{scheme}://127.0.0.1:{port}/");
    let padded_url = format!("{plain_url}?padding={padding}");
    let name = format!("official-interop-{}", uuid::Uuid::new_v4());
    let (custom, official) = if official_listens {
        (
            network_config(&name, None, Some(&padded_url)),
            network_config(&name, Some(&plain_url), None),
        )
    } else {
        (
            network_config(&name, Some(&padded_url), None),
            network_config(&name, None, Some(&plain_url)),
        )
    };
    let mut instance = TestInstance::new_with_process_runtime(
        TomlConfig::new_from_str(&custom)?,
        CoreProcessRuntime::new(),
    );
    instance.run().await.context("start custom native peer")?;
    let outcome = async {
        let mut process = OfficialProcess::start(&binary, Some(&official), &[])?;
        if official_listens {
            process.wait_listener(port).await?;
        }
        let core = instance.get_core_instance();
        let connection = timeout(DEADLINE, async {
            loop {
                process.ensure_running()?;
                let peers = core.peer_snapshots().await;
                if let Some(conn) = peers.iter().flat_map(|peer| &peer.conns).find(|conn| {
                    !conn.is_closed
                        && conn
                            .tunnel
                            .as_ref()
                            .is_some_and(|t| t.tunnel_type == scheme)
                }) {
                    return Ok::<_, anyhow::Error>(conn.conn_id.clone());
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "official/custom peer handshake timed out:\n{}",
                process.logs()
            )
        })??;
        // More than 20 actual padding periods must not kill or replace the connection.
        sleep(Duration::from_secs(2)).await;
        process.ensure_running()?;
        let peers = core.peer_snapshots().await;
        let conn = peers
            .iter()
            .flat_map(|peer| &peer.conns)
            .find(|conn| conn.conn_id == connection && !conn.is_closed)
            .with_context(|| {
                format!(
                    "peer connection did not survive padding intervals:\n{}",
                    process.logs()
                )
            })?;
        let stats = conn
            .stats
            .as_ref()
            .context("connected peer has no byte counters")?;
        ensure!(
            stats.rx_bytes > 0 && stats.tx_bytes > 0,
            "peer connection must exchange bytes in both directions"
        );
        Ok(())
    }
    .await;
    instance.clear_resources().await;
    outcome.with_context(|| {
        format!("{scheme}, official_listens={official_listens}, padding={padding}")
    })
}

#[derive(Clone)]
struct ManagementServer {
    heartbeats: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl WebServerService for ManagementServer {
    type Controller = BaseController;

    async fn heartbeat(
        &self,
        _: BaseController,
        request: HeartbeatRequest,
    ) -> rpc_types::error::Result<HeartbeatResponse> {
        assert_eq!(request.user_token, "interop-user");
        self.heartbeats.fetch_add(1, Ordering::SeqCst);
        Ok(HeartbeatResponse {
            heartbeat_interval_ms: Some(1000),
            heartbeat_timeout_ms: Some(6000),
        })
    }

    async fn get_feature(
        &self,
        _: BaseController,
        _: GetFeatureRequest,
    ) -> rpc_types::error::Result<GetFeatureResponse> {
        // Exercise classic config-server RPC without a Peer or Noise handshake.
        Ok(GetFeatureResponse {
            support_encryption: false,
        })
    }
}

async fn management_interop(scheme: &str) -> anyhow::Result<()> {
    let binary = official_binary()?;
    let mut listener =
        WsTunnelListener::new(format!("{scheme}://127.0.0.1:0/?padding=50,64").parse()?);
    listener.listen().await?;
    let mut server_url = listener.local_url();
    server_url.set_path("/interop-user");
    let mut process = OfficialProcess::start(
        &binary,
        None,
        &["--config-server".to_owned(), server_url.to_string()],
    )?;
    let tunnel = timeout(DEADLINE, listener.accept())
        .await
        .with_context(|| {
            format!(
                "official management connection timed out:\n{}",
                process.logs()
            )
        })??;
    let heartbeats = Arc::new(AtomicUsize::new(0));
    let rpc = BidirectRpcManager::new();
    rpc.rpc_server().registry().register(
        WebServerServiceServer::new(ManagementServer {
            heartbeats: heartbeats.clone(),
        }),
        "",
    );
    rpc.run_with_tunnel(tunnel);
    let outcome = async {
        timeout(DEADLINE, async {
            while heartbeats.load(Ordering::SeqCst) < 2 {
                process.ensure_running()?;
                sleep(Duration::from_millis(100)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("official config-server heartbeat failed")??;
        let client = rpc
            .rpc_client()
            .scoped_client::<WebClientServiceClientFactory<BaseController>>(1, 1, String::new());
        let response = timeout(
            DEADLINE,
            client.list_network_instance(
                BaseController {
                    timeout_ms: 5000,
                    ..Default::default()
                },
                ListNetworkInstanceRequest {},
            ),
        )
        .await
        .context("reverse config-server RPC timed out")??;
        ensure!(
            response.inst_ids.is_empty(),
            "management harness must not launch a network"
        );
        process.ensure_running()?;
        Ok(())
    }
    .await;
    rpc.stop().await;
    outcome.with_context(|| {
        format!(
            "official {scheme} config-server client:\n{}",
            process.logs()
        )
    })
}

#[tokio::test]
#[ignore = "requires independently built upstream CLI in OFFICIAL_EASYTIER_BIN"]
async fn official_ws_peer_both_directions() {
    for padding in ["true", "50,64"] {
        for official_listens in [true, false] {
            peer_interop("ws", official_listens, padding).await.unwrap();
        }
    }
}

#[tokio::test]
#[ignore = "requires independently built upstream CLI in OFFICIAL_EASYTIER_BIN"]
async fn official_wss_peer_both_directions() {
    for padding in ["true", "50,64"] {
        for official_listens in [true, false] {
            peer_interop("wss", official_listens, padding)
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
#[ignore = "requires independently built upstream CLI in OFFICIAL_EASYTIER_BIN"]
async fn official_ws_management_client() {
    management_interop("ws").await.unwrap();
}

#[tokio::test]
#[ignore = "requires independently built upstream CLI in OFFICIAL_EASYTIER_BIN"]
async fn official_wss_management_client() {
    management_interop("wss").await.unwrap();
}

#[tokio::test]
#[ignore = "requires independently built upstream CLI in OFFICIAL_EASYTIER_BIN"]
async fn official_listener_rejects_unsolicited_text_padding() {
    for scheme in ["ws", "wss"] {
        let binary = official_binary().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("{scheme}://127.0.0.1:{port}/");
        let config = network_config("official-text-rejection", Some(&url), None);
        let mut process = OfficialProcess::start(&binary, Some(&config), &[]).unwrap();
        process.wait_listener(port).await.unwrap();
        let socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let stream = if scheme == "wss" {
            init_crypto_provider();
            let tls = tokio_rustls::TlsConnector::from(Arc::new(get_insecure_tls_client_config()));
            let server_name =
                rustls::pki_types::ServerName::try_from("localhost".to_owned()).unwrap();
            MaybeTlsStream::Rustls(tls.connect(server_name, socket).await.unwrap())
        } else {
            MaybeTlsStream::Plain(socket)
        };
        let (mut client, response) = ClientBuilder::from_uri(url.parse::<http::Uri>().unwrap())
            .connect_on(stream)
            .await
            .unwrap();
        assert!(
            !response
                .headers()
                .contains_key(super::TRANSPORT_FEATURES_HEADER)
        );
        client
            .send(Message::text("cGFkZGluZw==".to_owned()))
            .await
            .unwrap();
        // The peer handshake has a five-second idle timeout. A text frame must
        // fail the official tunnel immediately, before that idle timeout.
        timeout(Duration::from_secs(3), async {
            while let Some(Ok(message)) = client.next().await {
                if message.is_close() {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{scheme} official listener did not reject text padding immediately:\n{}",
                process.logs()
            )
        });
        process.ensure_running().unwrap();
    }
}
