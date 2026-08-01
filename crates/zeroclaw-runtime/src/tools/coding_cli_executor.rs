use crate::platform::RuntimeAdapter;
use crate::security::traits::Sandbox;
use async_trait::async_trait;
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;
use zeroclaw_tools::coding_cli::{
    CodingCliCommand, CodingCliExecutionError, CodingCliExecutor, host_native_program,
};

pub(crate) struct RuntimeCodingCliExecutor {
    runtime: Arc<dyn RuntimeAdapter>,
    sandbox: Arc<dyn Sandbox>,
    use_native_argv: bool,
}

impl RuntimeCodingCliExecutor {
    pub(crate) fn shared(
        runtime: Arc<dyn RuntimeAdapter>,
        sandbox: Arc<dyn Sandbox>,
        use_native_argv: bool,
    ) -> Arc<dyn CodingCliExecutor> {
        Arc::new(Self {
            runtime,
            sandbox,
            use_native_argv,
        })
    }
}

#[async_trait]
impl CodingCliExecutor for RuntimeCodingCliExecutor {
    async fn output(&self, command: CodingCliCommand) -> Result<Output, CodingCliExecutionError> {
        let timeout_secs = command.timeout_secs;
        let mut process = if self.use_native_argv {
            native_command(&command)
        } else {
            self.runtime
                .build_shell_command(&shell_command(&command), &command.working_dir)
                .map_err(CodingCliExecutionError::Prepare)?
        };

        self.sandbox
            .wrap_command(process.as_std_mut())
            .map_err(|error| CodingCliExecutionError::Prepare(error.into()))?;

        process.env_clear();
        for (key, value) in command.env {
            process.env(key, value);
        }
        process.kill_on_drop(true);

        tokio::time::timeout(Duration::from_secs(timeout_secs), process.output())
            .await
            .map_err(|_| CodingCliExecutionError::Timeout)?
            .map_err(CodingCliExecutionError::Io)
    }
}

fn native_command(command: &CodingCliCommand) -> tokio::process::Command {
    let mut process = tokio::process::Command::new(host_native_program(&command.program));
    process.args(&command.args);
    process.current_dir(&command.working_dir);
    process
}

fn shell_command(command: &CodingCliCommand) -> String {
    std::iter::once(command.program.as_os_str())
        .chain(command.args.iter().map(|arg| arg.as_os_str()))
        .map(shell_escape)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '+'))
    {
        value.into_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecurityPolicy;
    use crate::security::traits::Sandbox;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use zeroclaw_api::runtime_traits::RuntimeAdapter;
    use zeroclaw_api::tool::Tool;
    use zeroclaw_config::autonomy::AutonomyLevel;
    use zeroclaw_config::schema::CodexCliConfig;
    use zeroclaw_tools::codex_cli::CodexCliTool;

    #[test]
    fn shell_command_uses_posix_quoting_for_shell_runtimes() {
        let mut command = CodingCliCommand::new("codex", PathBuf::from("."), 1);
        command.args(["exec", "hello world", "it's safe; really"]);

        let rendered = shell_command(&command);
        assert_eq!(rendered, "codex exec 'hello world' 'it'\\''s safe; really'");
    }

    #[cfg(not(target_os = "windows"))]
    struct FakeRuntime {
        seen_command: Arc<Mutex<Option<String>>>,
    }

    #[cfg(not(target_os = "windows"))]
    impl RuntimeAdapter for FakeRuntime {
        fn name(&self) -> &str {
            "fake-runtime"
        }

        fn has_shell_access(&self) -> bool {
            true
        }

        fn has_filesystem_access(&self) -> bool {
            true
        }

        fn storage_path(&self) -> PathBuf {
            PathBuf::from("/tmp/fake-runtime")
        }

        fn supports_long_running(&self) -> bool {
            true
        }

        fn build_shell_command(
            &self,
            command: &str,
            workspace_dir: &std::path::Path,
        ) -> anyhow::Result<tokio::process::Command> {
            *self.seen_command.lock().expect("fake runtime mutex") = Some(command.to_string());
            let mut process = tokio::process::Command::new("/bin/sh");
            process
                .args(["-c", "printf '%s' \"$1\"", "zc-runtime"])
                .current_dir(workspace_dir);
            Ok(process)
        }
    }

    #[cfg(not(target_os = "windows"))]
    struct FakeSandbox;

    #[cfg(not(target_os = "windows"))]
    impl Sandbox for FakeSandbox {
        fn wrap_command(&self, cmd: &mut std::process::Command) -> std::io::Result<()> {
            cmd.arg("sandboxed");
            Ok(())
        }

        fn is_available(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "fake-sandbox"
        }

        fn description(&self) -> &str {
            "test sandbox"
        }
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn codex_cli_uses_runtime_and_sandbox_executor() {
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let seen_command = Arc::new(Mutex::new(None));
        let runtime = Arc::new(FakeRuntime {
            seen_command: Arc::clone(&seen_command),
        });
        let executor = RuntimeCodingCliExecutor::shared(runtime, Arc::new(FakeSandbox), false);
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: workspace.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let tool = CodexCliTool::new_with_executor(
            security,
            CodexCliConfig {
                timeout_secs: 5,
                ..CodexCliConfig::default()
            },
            executor,
        );

        let result = tool
            .execute(json!({"prompt": "prove runtime boundary"}))
            .await
            .expect("codex_cli should return a tool result");

        assert!(result.success, "unexpected error: {:?}", result.error);
        assert_eq!(result.output.trim(), "sandboxed");
        let command = seen_command
            .lock()
            .expect("fake runtime mutex")
            .clone()
            .expect("runtime should receive the coding CLI command");
        assert!(command.contains("codex"), "command was {command:?}");
        assert!(command.contains("exec"), "command was {command:?}");
        assert!(
            command.contains("prove runtime boundary"),
            "command was {command:?}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn native_runtime_executes_argv_without_shell_interpretation() {
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let runtime = Arc::new(FakeRuntime {
            seen_command: Arc::new(Mutex::new(None)),
        });
        let executor = RuntimeCodingCliExecutor::shared(runtime, Arc::new(FakeSandbox), true);
        let mut command = CodingCliCommand::new("/bin/echo", workspace.path().to_path_buf(), 5);
        command.arg("hello; exit 7");

        let output = executor
            .output(command)
            .await
            .expect("native argv command should execute");

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello; exit 7 sandboxed"
        );
    }
}
