//! Retire other EasyTier services that would share the GUI core's config store
//! or RPC port. A service with unrelated resources is left alone.

use anyhow::{Context as _, Result};

/// Result of retiring one conflicting service. Reported per service so a
/// single failure cannot hide what already happened to the others.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RetireOutcome {
    pub name: String,
    /// "disabled" (stopped + start type disabled; reversible), "uninstalled"
    /// (irreversible; only with explicit user confirmation) or "failed".
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn rpc_bind_address(portal: &str) -> Result<std::net::SocketAddr> {
    let portal = normalize_rpc_portal(portal)?;
    if let Ok(port) = portal.parse::<u16>() {
        return Ok(std::net::SocketAddr::from(([0, 0, 0, 0], port)));
    }
    Ok(portal.parse()?)
}

#[cfg(any(target_os = "windows", test))]
fn rpc_bindings_overlap(left: std::net::SocketAddr, right: std::net::SocketAddr) -> bool {
    if left.port() != right.port() {
        return false;
    }
    match (left, right) {
        (std::net::SocketAddr::V4(left), std::net::SocketAddr::V4(right)) => {
            left.ip() == right.ip() || left.ip().is_unspecified() || right.ip().is_unspecified()
        }
        (std::net::SocketAddr::V6(left), std::net::SocketAddr::V6(right)) => {
            left.ip().is_unspecified()
                || right.ip().is_unspecified()
                || (left.ip() == right.ip() && left.scope_id() == right.scope_id())
        }
        // The core's RPC listener sets IPV6_V6ONLY.
        _ => false,
    }
}

pub fn normalize_rpc_portal(portal: &str) -> Result<String> {
    let portal = portal.trim();
    let portal = if portal
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("tcp://"))
    {
        portal[6..].trim_end_matches('/')
    } else {
        portal
    };
    if let Ok(port) = portal.parse::<u16>()
        && port != 0
    {
        return Ok(portal.to_owned());
    }
    if let Ok(address) = portal.parse::<std::net::SocketAddr>()
        && address.port() != 0
    {
        return Ok(address.to_string());
    }
    anyhow::bail!("service RPC portal must be a TCP IP:port or a nonzero port")
}

pub fn ensure_rpc_port_free(portal: &str) -> Result<()> {
    let address = rpc_bind_address(portal)?;
    std::net::TcpListener::bind(address).with_context(|| {
        format!("RPC portal {address} is occupied; stop the process using it before starting the GUI service")
    })?;
    Ok(())
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use easytier::service_manager::{Service, ServiceStatus};
    use std::{
        os::windows::process::CommandExt as _,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    use winreg::{
        RegKey,
        enums::{HKEY_LOCAL_MACHINE, KEY_READ},
    };

    fn arguments(command_line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for ch in command_line.chars() {
            match ch {
                '"' => quoted = !quoted,
                c if c.is_whitespace() && !quoted => {
                    if !current.is_empty() {
                        args.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            args.push(current);
        }
        args
    }

    fn executable_name(command_line: &str) -> String {
        let command_line = command_line.trim_start();
        let executable = if let Some(quoted) = command_line.strip_prefix('"') {
            quoted.split('"').next().unwrap_or_default()
        } else {
            let lowercase = command_line.to_ascii_lowercase();
            let end = lowercase.find(".exe").map(|index| index + 4).unwrap_or(0);
            &command_line[..end]
        };
        Path::new(executable)
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    }

    fn option_value<'a>(args: &'a [String], option: &str) -> Option<&'a str> {
        for (index, arg) in args.iter().enumerate() {
            if arg.eq_ignore_ascii_case(option) {
                return args.get(index + 1).map(String::as_str);
            }
            if let Some((key, value)) = arg.split_once('=') {
                if key.eq_ignore_ascii_case(option) {
                    return Some(value);
                }
            }
        }
        None
    }

    fn normalized_path(path: &Path) -> String {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let raw = path.to_string_lossy().replace('/', "\\");
        raw.strip_prefix("\\\\?\\")
            .unwrap_or(&raw)
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    }

    fn service_config_dir(args: &[String], work_dir: Option<&Path>) -> Option<PathBuf> {
        let configured = PathBuf::from(option_value(args, "--config-dir")?);
        if configured.is_absolute() {
            Some(configured)
        } else {
            work_dir.map(|dir| dir.join(configured))
        }
    }

    fn is_conflicting(
        name: &str,
        image_path: &str,
        work_dir: Option<&Path>,
        desired_dir: &Path,
        desired_address: Option<std::net::SocketAddr>,
    ) -> bool {
        if name.eq_ignore_ascii_case(env!("CARGO_PKG_NAME")) {
            return false;
        }
        let args = arguments(image_path);
        let executable = executable_name(image_path);
        let is_easytier =
            executable.contains("easytier-core") || executable.contains("easytier-gui");
        if !is_easytier {
            return false;
        }
        let shares_dir = service_config_dir(&args, work_dir)
            .is_some_and(|dir| normalized_path(&dir) == normalized_path(desired_dir));
        let shares_port = desired_address.is_some_and(|address| {
            option_value(&args, "--rpc-portal")
                .and_then(|value| rpc_bind_address(value).ok())
                .is_some_and(|other| rpc_bindings_overlap(address, other))
        });
        shares_dir || shares_port
    }

    fn service_work_dir(name: &str) -> Option<PathBuf> {
        let key = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(
                easytier::common::constants::WIN_SERVICE_WORK_DIR_REG_KEY,
                KEY_READ,
            )
            .ok()?;
        key.get_value::<String, _>(name).ok().map(PathBuf::from)
    }

    fn force_retire(name: &str) -> Result<()> {
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS)?;
        let service = manager.open_service(name, ServiceAccess::ALL_ACCESS)?;
        let pid = service.query_status()?.process_id;
        // Mark for deletion before force termination so SCM recovery cannot restart it.
        service.delete()?;
        if let Some(pid) = pid {
            let system_root = std::env::var_os("SystemRoot").context("SystemRoot is not set")?;
            let taskkill = PathBuf::from(system_root)
                .join("System32")
                .join("taskkill.exe");
            let result = std::process::Command::new(taskkill)
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .status()
                .with_context(|| format!("failed to terminate stuck EasyTier service {name}"))?;
            if !result.success() && service.query_status()?.current_state != ServiceState::Stopped {
                anyhow::bail!("failed to terminate stuck EasyTier service {name} (PID {pid})");
            }
        }
        Ok(())
    }

    fn stop_service_within(name: &str, service: &Service, timeout: Duration) -> Result<()> {
        if service.status()? != ServiceStatus::Running {
            return Ok(());
        }
        service.stop()?;
        let deadline = Instant::now() + timeout;
        while service.status()? == ServiceStatus::Running {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "service {name} did not stop within {}s",
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    /// Preferred, reversible retirement: stop the service and disable its
    /// start type via `sc.exe config` (windows-service 0.8 cannot change only
    /// the start type without rewriting the whole launch command). The
    /// service stays installed and the user can re-enable it at any time.
    fn disable_service(name: &str) -> Result<()> {
        let service = Service::new(name.to_string())
            .with_context(|| format!("failed to open conflicting EasyTier service {name}"))?;
        stop_service_within(name, &service, Duration::from_secs(30))?;
        let system_root = std::env::var_os("SystemRoot").context("SystemRoot is not set")?;
        let sc = PathBuf::from(system_root).join("System32").join("sc.exe");
        let result = std::process::Command::new(sc)
            .args(["config", name, "start=", "disabled"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .with_context(|| format!("failed to run sc.exe for service {name}"))?;
        if !result.status.success() {
            anyhow::bail!(
                "sc.exe failed to disable service {name}: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            );
        }
        Ok(())
    }

    fn retire_one(name: &str, allow_uninstall: bool) -> Result<&'static str> {
        if let Err(error) = disable_service(name) {
            if !allow_uninstall {
                return Err(error.context(format!(
                    "failed to stop and disable conflicting EasyTier service {name} (removal requires confirmation)"
                )));
            }
        } else {
            return Ok("disabled");
        }

        // Irreversible fallback, only reachable with explicit user
        // confirmation: uninstall the service and force-terminate a stuck
        // process.
        let service = Service::new(name.to_string())
            .with_context(|| format!("failed to open conflicting EasyTier service {name}"))?;
        let force_needed = service.status()? == ServiceStatus::Running
            && stop_service_within(name, &service, Duration::from_secs(30)).is_err();
        if force_needed {
            force_retire(name)?;
            return Ok("uninstalled");
        }
        if service.status()? != ServiceStatus::NotInstalled {
            service
                .uninstall()
                .or_else(|_| force_retire(name))
                .with_context(|| {
                    format!("failed to uninstall conflicting EasyTier service {name}")
                })?;
        }
        Ok("uninstalled")
    }

    pub(super) fn retire_conflicting_services(
        config_dir: &Path,
        portal: Option<&str>,
        allow_uninstall: bool,
    ) -> Result<Vec<RetireOutcome>> {
        let desired_address = portal.map(rpc_bind_address).transpose()?;
        let services = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags("SYSTEM\\CurrentControlSet\\Services", KEY_READ)
            .context("failed to enumerate Windows services")?;
        let mut outcomes = Vec::new();
        for name in services.enum_keys().flatten() {
            let Ok(key) = services.open_subkey_with_flags(&name, KEY_READ) else {
                continue;
            };
            let Ok(image_path) = key.get_value::<String, _>("ImagePath") else {
                continue;
            };
            let work_dir = service_work_dir(&name);
            if !is_conflicting(
                &name,
                &image_path,
                work_dir.as_deref(),
                config_dir,
                desired_address,
            ) {
                continue;
            }

            // One service's failure must not abort the loop: the caller needs
            // to know the state of every service, including the ones already
            // handled before the failure.
            let outcome = match retire_one(&name, allow_uninstall) {
                Ok(action) => RetireOutcome {
                    name: name.clone(),
                    action: action.to_owned(),
                    error: None,
                },
                Err(error) => RetireOutcome {
                    name: name.clone(),
                    action: "failed".to_owned(),
                    error: Some(format!("{error:#}")),
                },
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn finds_conflicts_by_resources_without_a_fixed_service_name() {
            let target = Path::new("C:/shared/config.d");
            assert!(is_conflicting(
                "custom-vpn-service",
                "\"C:/tools/easytier-core.exe\" --config-dir \"C:/shared/config.d\" --rpc-portal 127.0.0.1:15999",
                None,
                target,
                Some("127.0.0.1:15999".parse().unwrap())
            ));
            assert!(is_conflicting(
                "another-easytier",
                "C:/tools/easytier-core.exe --config-dir C:/other --rpc-portal 0.0.0.0:15999",
                None,
                target,
                Some("127.0.0.1:15999".parse().unwrap())
            ));
            assert!(is_conflicting(
                "renamed-service",
                "C:/Program Files/EasyTier/easytier-core.exe --config-dir C:/shared/config.d",
                None,
                target,
                None
            ));
            assert!(!is_conflicting(
                "other-vpn",
                "C:/tools/other-core.exe --config-dir C:/shared/config.d --rpc-portal 127.0.0.1:15999",
                None,
                target,
                Some("127.0.0.1:15999".parse().unwrap())
            ));
            assert!(!is_conflicting(
                "easytier-gui",
                "C:/tools/easytier-gui.exe --config-dir C:/shared/config.d",
                None,
                target,
                Some("127.0.0.1:15999".parse().unwrap())
            ));
        }

        #[test]
        fn independent_rpc_addresses_do_not_retire_another_service() {
            let image =
                "C:/tools/easytier-core.exe --config-dir C:/other --rpc-portal 127.0.0.2:15999";
            let address = Some("127.0.0.1:15999".parse().unwrap());
            assert!(!is_conflicting(
                "another-easytier",
                image,
                None,
                Path::new("C:/shared/config.d"),
                address
            ));
            assert!(is_conflicting(
                "another-easytier",
                image,
                None,
                Path::new("C:/other"),
                address
            ));
        }
    }
}

pub fn retire_conflicting_services(
    config_dir: &std::path::Path,
    portal: Option<&str>,
    allow_uninstall: bool,
) -> Result<Vec<RetireOutcome>> {
    #[cfg(target_os = "windows")]
    {
        windows::retire_conflicting_services(config_dir, portal, allow_uninstall)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (config_dir, portal, allow_uninstall);
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_rpc_portal_is_normalized_for_the_core_cli() {
        assert_eq!(
            normalize_rpc_portal("tcp://0.0.0.0:15999").unwrap(),
            "0.0.0.0:15999"
        );
        assert_eq!(
            normalize_rpc_portal("TCP://127.0.0.1:15999").unwrap(),
            "127.0.0.1:15999"
        );
        assert_eq!(normalize_rpc_portal("[::1]:15999").unwrap(), "[::1]:15999");
        assert_eq!(normalize_rpc_portal("15999").unwrap(), "15999");
        assert!(normalize_rpc_portal("udp://127.0.0.1:15999").is_err());
        assert!(normalize_rpc_portal("127.0.0.1:0").is_err());
    }

    #[test]
    fn occupied_rpc_port_is_rejected_before_service_start() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        assert!(ensure_rpc_port_free(&address.to_string()).is_err());
        drop(listener);
        assert!(ensure_rpc_port_free(&address.to_string()).is_ok());
    }

    #[test]
    fn rpc_conflicts_require_overlapping_bindings() {
        for (left, right, expected) in [
            ("127.0.0.1:15999", "127.0.0.1:15999", true),
            ("127.0.0.1:15999", "127.0.0.2:15999", false),
            ("127.0.0.1:15999", "127.0.0.1:16000", false),
            ("0.0.0.0:15999", "127.0.0.2:15999", true),
            ("15999", "127.0.0.2:15999", true),
            ("[::]:15999", "[::1]:15999", true),
            ("[::1]:15999", "[::2]:15999", false),
            ("[::]:15999", "0.0.0.0:15999", false),
            ("[::ffff:127.0.0.1]:15999", "127.0.0.1:15999", false),
            ("[fe80::1%2]:15999", "[fe80::1%3]:15999", false),
            ("[fe80::1%2]:15999", "[fe80::1%2]:15999", true),
        ] {
            let left = rpc_bind_address(left).unwrap();
            let right = rpc_bind_address(right).unwrap();
            assert_eq!(
                rpc_bindings_overlap(left, right),
                expected,
                "{left} vs {right}"
            );
            assert_eq!(
                rpc_bindings_overlap(right, left),
                expected,
                "{right} vs {left}"
            );
        }
    }
}
