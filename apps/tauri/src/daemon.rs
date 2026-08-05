//! Locate and launch a `zeroclaw daemon` when none is already running.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use std::{io::BufRead, io::Read};

const CAPTURE_READY: &str = "ZEROCLAW_SERVICE_CAPTURE_READY";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureStatus {
    Active,
    Inactive,
}

/// Filename of the kernel binary on the current platform.
fn zeroclaw_exe_name() -> &'static str {
    if cfg!(windows) {
        "zeroclaw.exe"
    } else {
        "zeroclaw"
    }
}

/// Find the `zeroclaw` binary. Checks, in order: the directory next to this
/// app (installed side-by-side), every `PATH` entry, then the common install
/// locations a GUI launch's minimal `PATH` usually misses.
pub fn find_zeroclaw_binary() -> Option<PathBuf> {
    let exe_name = zeroclaw_exe_name();

    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name(exe_name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }

    // 2. Any directory on PATH.
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(exe_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 3. Common install locations (Finder/Dock launches inherit a minimal PATH
    //    that usually omits ~/.cargo/bin and the Homebrew prefixes).
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for rel in [".cargo/bin", ".local/bin"] {
            let candidate = home.join(rel).join(exe_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        let candidate = Path::new(dir).join(exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

/// Spawn the internal bounded daemon runner, detached so it outlives the app.
/// The child handle is returned but intentionally not reaped because the
/// runner owns the background daemon lifecycle.
pub fn spawn_daemon(binary: &Path, port: u16) -> std::io::Result<Child> {
    let mut cmd = daemon_command(binary, port);
    cmd.arg("--ready-signal");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Detach so signals to the app's process group (e.g. Ctrl-C on a dev
    // `cargo run`) don't also stop the daemon, and so it survives app exit.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        cmd.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }

    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("daemon readiness pipe unavailable"))?;
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .map(|_| line);
        let _ = ready_tx.send(result);
    });

    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(line)) if line.trim() == CAPTURE_READY => {
            if let Some(mut stderr) = child.stderr.take() {
                std::thread::spawn(move || {
                    let mut discard = [0_u8; 1024];
                    while stderr.read(&mut discard).is_ok_and(|read| read > 0) {}
                });
            }
            Ok(child)
        }
        outcome => {
            let _ = child.kill();
            let _ = child.wait();
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            let detail = match outcome {
                Ok(Ok(line)) if !line.trim().is_empty() => line.trim().to_string(),
                Ok(Ok(_)) => stderr.trim().to_string(),
                Ok(Err(error)) => error.to_string(),
                Err(_) => "timed out waiting for bounded capture".to_string(),
            };
            Err(std::io::Error::other(format!(
                "daemon log capture did not become ready: {detail}"
            )))
        }
    }
}

pub fn capture_status(binary: &Path, port: u16) -> std::io::Result<CaptureStatus> {
    let output = daemon_command(binary, port)
        .arg("--capture-status")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "could not inspect daemon log capture: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "active" => Ok(CaptureStatus::Active),
        "inactive" => Ok(CaptureStatus::Inactive),
        other => Err(std::io::Error::other(format!(
            "unexpected daemon capture status: {other}"
        ))),
    }
}

fn daemon_command(binary: &Path, port: u16) -> Command {
    let mut cmd = Command::new(binary);
    cmd.arg("service")
        .arg("run-daemon")
        .arg("--desktop")
        .arg("--port")
        .arg(port.to_string());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_uses_bounded_service_runner() {
        let command = daemon_command(Path::new("/tmp/zeroclaw"), 42617);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["service", "run-daemon", "--desktop", "--port", "42617"]
        );
    }

    #[test]
    fn desktop_status_uses_hidden_capture_check() {
        let mut command = daemon_command(Path::new("/tmp/zeroclaw"), 42617);
        command.arg("--capture-status");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().map(String::as_str), Some("--capture-status"));
    }

    #[test]
    fn desktop_actual_runner_emits_readiness() {
        let mut command = daemon_command(Path::new("/tmp/zeroclaw"), 42617);
        command.arg("--ready-signal");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().map(String::as_str), Some("--ready-signal"));
    }
}
