use async_trait::async_trait;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct CodingCliCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub working_dir: PathBuf,
    pub timeout_secs: u64,
}

impl CodingCliCommand {
    pub fn new(program: impl Into<OsString>, working_dir: PathBuf, timeout_secs: u64) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            working_dir,
            timeout_secs,
        }
    }

    pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(&mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> &mut Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodingCliExecutionError {
    #[error("failed to execute command: {0}")]
    Io(#[from] std::io::Error),
    #[error("command timed out")]
    Timeout,
    #[error("failed to prepare command: {0}")]
    Prepare(#[from] anyhow::Error),
}

#[async_trait]
pub trait CodingCliExecutor: Send + Sync {
    async fn output(&self, command: CodingCliCommand) -> Result<Output, CodingCliExecutionError>;
}

#[derive(Debug, Default)]
pub struct DirectCodingCliExecutor;

impl DirectCodingCliExecutor {
    pub fn shared() -> Arc<dyn CodingCliExecutor> {
        Arc::new(Self)
    }
}

#[async_trait]
impl CodingCliExecutor for DirectCodingCliExecutor {
    async fn output(&self, command: CodingCliCommand) -> Result<Output, CodingCliExecutionError> {
        let mut process = Command::new(host_native_program(&command.program));
        process.args(&command.args);
        process.env_clear();
        for (key, value) in command.env {
            process.env(key, value);
        }
        process.current_dir(command.working_dir);
        process.kill_on_drop(true);

        tokio::time::timeout(Duration::from_secs(command.timeout_secs), process.output())
            .await
            .map_err(|_| CodingCliExecutionError::Timeout)?
            .map_err(CodingCliExecutionError::Io)
    }
}

pub fn host_native_program(program: &OsStr) -> OsString {
    if cfg!(target_os = "windows") {
        host_native_windows_program(program)
    } else {
        program.to_os_string()
    }
}

fn host_native_windows_program(program: &OsStr) -> OsString {
    host_native_windows_program_with(program, || {
        which::which("claude")
            .ok()
            .map(|path| path.into_os_string())
    })
}

fn host_native_windows_program_with<F>(program: &OsStr, find_claude: F) -> OsString
where
    F: FnOnce() -> Option<OsString>,
{
    match program.to_str() {
        Some("codex") => OsString::from("codex.cmd"),
        Some("gemini") => OsString::from("gemini.cmd"),
        Some("claude") => find_claude().unwrap_or_else(|| OsString::from("claude.cmd")),
        _ => program.to_os_string(),
    }
}

pub fn add_safe_env(command: &mut CodingCliCommand, safe_vars: &[&str], passthrough: &[String]) {
    for var in safe_vars {
        if let Ok(val) = std::env::var(var) {
            command.env(*var, val);
        }
    }
    for var in passthrough {
        let trimmed = var.trim();
        if !trimmed.is_empty()
            && let Ok(val) = std::env::var(trimmed)
        {
            command.env(trimmed, val);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_native_windows_program_preserves_known_cli_shims() {
        assert_eq!(
            host_native_windows_program_with(OsStr::new("codex"), || None),
            OsString::from("codex.cmd")
        );
        assert_eq!(
            host_native_windows_program_with(OsStr::new("gemini"), || None),
            OsString::from("gemini.cmd")
        );
        assert_eq!(
            host_native_windows_program_with(OsStr::new("claude"), || None),
            OsString::from("claude.cmd")
        );
        assert_eq!(
            host_native_windows_program_with(OsStr::new("claude"), || Some(OsString::from(
                r"C:\Tools\claude.exe"
            ))),
            OsString::from(r"C:\Tools\claude.exe")
        );
        assert_eq!(
            host_native_windows_program_with(OsStr::new("opencode"), || None),
            OsString::from("opencode")
        );
    }

    #[test]
    fn host_native_program_leaves_names_neutral_on_non_windows() {
        if !cfg!(target_os = "windows") {
            assert_eq!(
                host_native_program(OsStr::new("codex")),
                OsString::from("codex")
            );
        }
    }
}
