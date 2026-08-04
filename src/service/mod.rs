pub use zeroclaw_runtime::service::*;

use crate::config::Config;
use anyhow::{Context, Result};

#[allow(dead_code)]
pub async fn handle_command(
    command: &crate::ServiceCommands,
    config: &Config,
    init_system: InitSystem,
) -> Result<()> {
    match command {
        crate::ServiceCommands::RunDaemon {
            desktop,
            port,
            preflight,
        } => {
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
            if *preflight {
                check_daemon_capture(config, init_system, profile).await
            } else {
                run_daemon(config, init_system, profile).await
            }
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
