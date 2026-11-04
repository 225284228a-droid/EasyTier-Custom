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
