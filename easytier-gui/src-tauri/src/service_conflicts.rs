//! Retire other EasyTier services that would share the GUI core's config store
//! or RPC port. A service with unrelated resources is left alone.

use anyhow::{Context as _, Result};

#[cfg(target_os = "windows")]
pub fn rpc_port(portal: &str) -> Result<u16> {
    let portal = normalize_rpc_portal(portal)?;
    if let Ok(port) = portal.parse::<u16>() {
        return Ok(port);
    }
    Ok(portal.parse::<std::net::SocketAddr>()?.port())
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
    let portal = normalize_rpc_portal(portal)?;
    let address = if let Ok(port) = portal.parse::<u16>() {
        std::net::SocketAddr::from(([0, 0, 0, 0], port))
    } else {
        portal.parse::<std::net::SocketAddr>()?
    };
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
        desired_port: Option<u16>,
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
        let shares_port = desired_port.is_some_and(|port| {
            option_value(&args, "--rpc-portal").and_then(|value| rpc_port(value).ok()) == Some(port)
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

    pub(super) fn retire_conflicting_services(
        config_dir: &Path,
        portal: Option<&str>,
    ) -> Result<Vec<String>> {
        let desired_port = portal.map(rpc_port).transpose()?;
        let services = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags("SYSTEM\\CurrentControlSet\\Services", KEY_READ)
            .context("failed to enumerate Windows services")?;
        let mut retired = Vec::new();
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
                desired_port,
            ) {
                continue;
            }

            let service = Service::new(name.clone())
                .with_context(|| format!("failed to open conflicting EasyTier service {name}"))?;
            let mut force_needed = false;
            if service.status()? == ServiceStatus::Running {
                if service.stop().is_err() {
                    force_needed = true;
                }
                let deadline = Instant::now() + Duration::from_secs(30);
                while !force_needed && service.status()? == ServiceStatus::Running {
                    if Instant::now() >= deadline {
                        force_needed = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            if force_needed {
                force_retire(&name)?;
                retired.push(name);
                continue;
            }
            if service.status()? != ServiceStatus::NotInstalled {
                service
                    .uninstall()
                    .or_else(|_| force_retire(&name))
                    .with_context(|| {
                        format!("failed to uninstall conflicting EasyTier service {name}")
                    })?;
                retired.push(name);
            }
        }
        Ok(retired)
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
                Some(15999)
            ));
            assert!(is_conflicting(
                "another-easytier",
                "C:/tools/easytier-core.exe --config-dir C:/other --rpc-portal 0.0.0.0:15999",
                None,
                target,
                Some(15999)
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
                Some(15999)
            ));
            assert!(!is_conflicting(
                "easytier-gui",
                "C:/tools/easytier-gui.exe --config-dir C:/shared/config.d",
                None,
                target,
                Some(15999)
            ));
        }
    }
}

pub fn retire_conflicting_services(
    config_dir: &std::path::Path,
    portal: Option<&str>,
) -> Result<Vec<String>> {
    #[cfg(target_os = "windows")]
    {
        windows::retire_conflicting_services(config_dir, portal)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (config_dir, portal);
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
}
