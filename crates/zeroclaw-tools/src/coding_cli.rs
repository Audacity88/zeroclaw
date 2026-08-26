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
    pub runtime_env_keys: Vec<OsString>,
    pub working_dir: PathBuf,
    pub timeout_secs: u64,
}

impl CodingCliCommand {
    pub fn new(program: impl Into<OsString>, working_dir: PathBuf, timeout_secs: u64) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            runtime_env_keys: Vec::new(),
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

    pub fn runtime_env_key(&mut self, key: impl Into<OsString>) -> &mut Self {
        let key = key.into();
        if !self.runtime_env_keys.contains(&key) {
            self.runtime_env_keys.push(key);
        }
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
        let mut process = Command::new(host_native_program_for_command(&command)?);
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

pub fn host_native_program(program: &OsStr) -> Result<OsString, CodingCliExecutionError> {
    if cfg!(target_os = "windows") {
        host_native_windows_program(program)
    } else {
        host_native_non_windows_program(program)
    }
}

pub fn host_native_program_for_command(
    command: &CodingCliCommand,
) -> Result<OsString, CodingCliExecutionError> {
    #[cfg(target_os = "windows")]
    {
        host_native_windows_program(&command.program)
    }

    #[cfg(not(target_os = "windows"))]
    {
        match command
            .env
            .iter()
            .rev()
            .find(|(key, _)| key == OsStr::new("PATH"))
        {
            Some((_, path)) => host_native_non_windows_program_with_path(
                &command.program,
                std::env::split_paths(path),
            ),
            None => host_native_non_windows_program(&command.program),
        }
    }
}

fn find_claude_on_path() -> Option<OsString> {
    which::which("claude")
        .ok()
        .map(|path| path.into_os_string())
}

fn host_native_non_windows_program(program: &OsStr) -> Result<OsString, CodingCliExecutionError> {
    zeroclaw_config::platform::resolve_executable(program)
        .map(|path| path.into_os_string())
        .map_err(|error| {
            let detail = error.to_string();
            CodingCliExecutionError::Prepare(anyhow::Error::new(error).context(format!(
                "Unix coding CLI executable {program:?} could not be resolved before applying the workspace working directory: {detail}"
            )))
        })
}

#[cfg(not(target_os = "windows"))]
fn host_native_non_windows_program_with_path<I, P>(
    program: &OsStr,
    path_entries: I,
) -> Result<OsString, CodingCliExecutionError>
where
    I: IntoIterator<Item = P>,
    P: AsRef<std::path::Path>,
{
    zeroclaw_config::platform::resolve_executable_with_path(program, path_entries)
        .map(|path| path.into_os_string())
        .map_err(|error| {
            let detail = error.to_string();
            CodingCliExecutionError::Prepare(anyhow::Error::new(error).context(format!(
                "Unix coding CLI executable {program:?} could not be resolved before applying the workspace working directory: {detail}"
            )))
        })
}

fn host_native_windows_program(program: &OsStr) -> Result<OsString, CodingCliExecutionError> {
    host_native_windows_program_with(program, find_claude_on_path)
}

fn host_native_windows_program_with<F>(
    program: &OsStr,
    find_claude: F,
) -> Result<OsString, CodingCliExecutionError>
where
    F: FnOnce() -> Option<OsString>,
{
    Ok(match program.to_str() {
        Some("codex") => OsString::from("codex.cmd"),
        Some("gemini") => OsString::from("gemini.cmd"),
        Some("claude") => find_claude().unwrap_or_else(|| OsString::from("claude.cmd")),
        _ => program.to_os_string(),
    })
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
            command.runtime_env_key(trimmed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_native_windows_program_preserves_known_cli_shims() {
        assert_eq!(
            host_native_windows_program_with(OsStr::new("codex"), || None).unwrap(),
            OsString::from("codex.cmd")
        );
        assert_eq!(
            host_native_windows_program_with(OsStr::new("gemini"), || None).unwrap(),
            OsString::from("gemini.cmd")
        );
        assert_eq!(
            host_native_windows_program_with(OsStr::new("claude"), || None).unwrap(),
            OsString::from("claude.cmd")
        );
        assert_eq!(
            host_native_windows_program_with(OsStr::new("claude"), || Some(OsString::from(
                r"C:\Tools\claude.exe"
            )))
            .unwrap(),
            OsString::from(r"C:\Tools\claude.exe")
        );
        assert_eq!(
            host_native_windows_program_with(OsStr::new("opencode"), || None).unwrap(),
            OsString::from("opencode")
        );
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn host_native_program_leaves_names_neutral_on_non_windows() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("codex");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            host_native_non_windows_program_with_path(OsStr::new("codex"), [dir.path()]).unwrap(),
            program.canonicalize().unwrap().into_os_string()
        );
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn host_native_non_windows_program_resolves_every_program_before_workspace_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("claude");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            host_native_non_windows_program_with_path(OsStr::new("claude"), [dir.path()]).unwrap(),
            program.canonicalize().unwrap().into_os_string()
        );
        let error = host_native_non_windows_program_with_path(
            OsStr::new("missing-coding-cli"),
            [dir.path()],
        )
        .expect_err("unresolved Unix Claude path should fail closed");
        assert!(
            error.to_string().contains("was not found on PATH"),
            "unexpected error: {error}"
        );
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn command_specific_path_is_authoritative_for_launcher_resolution() {
        let first = tempfile::tempdir().unwrap();
        let selected = tempfile::tempdir().unwrap();
        let program = selected.path().join("codex");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut command = CodingCliCommand::new("codex", PathBuf::from("."), 5);
        command.env("PATH", first.path().as_os_str());
        command.env("PATH", selected.path().as_os_str());

        assert_eq!(
            host_native_program_for_command(&command).unwrap(),
            program.canonicalize().unwrap().into_os_string()
        );
    }

    #[test]
    fn runtime_env_keys_are_explicit_and_deduplicated() {
        let mut command = CodingCliCommand::new("codex", PathBuf::from("."), 5);

        command.env("PATH", "/usr/bin");
        command.runtime_env_key("OPENAI_API_KEY");
        command.runtime_env_key("OPENAI_API_KEY");

        assert_eq!(
            command.env,
            vec![(OsString::from("PATH"), OsString::from("/usr/bin"))]
        );
        assert_eq!(
            command.runtime_env_keys,
            vec![OsString::from("OPENAI_API_KEY")]
        );
    }
}
