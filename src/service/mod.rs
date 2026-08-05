pub use zeroclaw_runtime::service::*;

use crate::config::Config;
use anyhow::{Context, Result};
use std::path::Path;

#[allow(dead_code)]
pub async fn handle_run_daemon(
    command: &crate::ServiceCommands,
    config_dir: Option<&Path>,
    init_system: InitSystem,
) -> Result<()> {
    let crate::ServiceCommands::RunDaemon {
        desktop,
        port,
        capture_status,
        ready_signal,
    } = command
    else {
        anyhow::bail!("internal service runner requires run-daemon");
    };
    let profile = if *desktop {
        ServiceDaemonProfile::Desktop {
            port: port.context("internal desktop runner requires --port")?,
        }
    } else {
        if port.is_some() {
            anyhow::bail!("--port is only valid with the internal desktop runner");
        }
        ServiceDaemonProfile::Service
    };
    if *capture_status {
        println!(
            "{}",
            if daemon_capture_is_active(config_dir, init_system, profile)? {
                "active"
            } else {
                "inactive"
            }
        );
        return Ok(());
    }
    run_daemon(config_dir, init_system, profile, *ready_signal).await
}

#[allow(dead_code)]
pub async fn handle_command(
    command: &crate::ServiceCommands,
    config: &Config,
    init_system: InitSystem,
) -> Result<()> {
    match command {
        crate::ServiceCommands::RunDaemon { .. } => {
            anyhow::bail!("internal service runner must dispatch before config loading")
        }
        crate::ServiceCommands::Install => install(config, init_system),
        crate::ServiceCommands::Start => start(config, init_system),
        crate::ServiceCommands::Stop => stop(config, init_system),
        crate::ServiceCommands::Restart => restart(config, init_system),
        crate::ServiceCommands::Status => status(config, init_system),
        crate::ServiceCommands::Uninstall => uninstall(config, init_system),
        crate::ServiceCommands::Logs { lines, follow } => {
            logs(config, init_system, *lines, *follow)
        }
    }
}
