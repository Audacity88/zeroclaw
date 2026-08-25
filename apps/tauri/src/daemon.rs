//! Locate and launch a bounded desktop daemon supervisor when none is already running.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
#[cfg(any(unix, windows))]
use std::time::Instant;

const READINESS_FRAME_MAX_BYTES: usize = 4096;

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;
#[cfg(unix)]
const ESRCH: i32 = 3;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
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

/// Spawn the bounded desktop daemon supervisor, detached so it outlives the app.
/// The child handle is returned but intentionally not reaped because the
/// supervisor owns the daemon's background lifecycle and log capture.
pub fn spawn_daemon(binary: &Path, port: u16) -> std::io::Result<Child> {
    let mut cmd = desktop_daemon_command(binary, port);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    // Detach so signals to the app's process group (e.g. Ctrl-C on a dev
    // `cargo run`) don't also stop the supervisor, and so it survives app exit.
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
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let startup_error = std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "desktop supervisor stdout unavailable",
            );
            return Err(attach_cleanup_error(
                startup_error,
                terminate_supervisor_tree(&mut child),
            ));
        }
    };
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read_readiness_frame(stdout));
    });
    let frame = match receiver.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let startup_error = std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "desktop supervisor readiness timed out",
            );
            return Err(attach_cleanup_error(
                startup_error,
                terminate_supervisor_tree(&mut child),
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let startup_error = std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "desktop supervisor readiness reader exited unexpectedly",
            );
            return Err(attach_cleanup_error(
                startup_error,
                terminate_supervisor_tree(&mut child),
            ));
        }
    };
    let line = match frame {
        Ok(Some(line)) => line,
        Ok(None) => {
            let status = child.try_wait()?;
            let detail = status
                .map(|status| format!("desktop supervisor exited before readiness ({status})"))
                .unwrap_or_else(|| "desktop supervisor closed readiness pipe".to_string());
            let startup_error = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, detail);
            return Err(attach_cleanup_error(
                startup_error,
                terminate_supervisor_tree(&mut child),
            ));
        }
        Err(error) => {
            return Err(attach_cleanup_error(
                error,
                terminate_supervisor_tree(&mut child),
            ));
        }
    };
    match parse_readiness_line(&line) {
        Ok(()) => Ok(child),
        Err(detail) => Err(attach_cleanup_error(
            std::io::Error::other(detail),
            terminate_supervisor_tree(&mut child),
        )),
    }
}

fn read_readiness_frame<R: Read>(mut reader: R) -> std::io::Result<Option<String>> {
    let mut frame = Vec::with_capacity(READINESS_FRAME_MAX_BYTES + 1);
    let bytes_read = std::io::BufReader::new(&mut reader)
        .take((READINESS_FRAME_MAX_BYTES + 1) as u64)
        .read_until(b'\n', &mut frame)?;
    if bytes_read == 0 {
        return Ok(None);
    }
    if frame.len() > READINESS_FRAME_MAX_BYTES && frame.last().copied() != Some(b'\n')
        || frame.len() > READINESS_FRAME_MAX_BYTES + 1
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("desktop supervisor readiness exceeded {READINESS_FRAME_MAX_BYTES} bytes"),
        ));
    }
    if frame.last().copied() != Some(b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "desktop supervisor readiness ended before newline",
        ));
    }
    frame.pop();
    String::from_utf8(frame).map(Some).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "desktop supervisor readiness was not valid UTF-8",
        )
    })
}

fn attach_cleanup_error(
    startup_error: std::io::Error,
    cleanup_result: std::io::Result<()>,
) -> std::io::Error {
    match cleanup_result {
        Ok(()) => startup_error,
        Err(cleanup_error) => std::io::Error::new(
            startup_error.kind(),
            format!("{startup_error}; supervisor cleanup failed: {cleanup_error}"),
        ),
    }
}

fn terminate_supervisor_tree(child: &mut Child) -> std::io::Result<()> {
    let mut utility_errors = Vec::new();
    let mut forceful_termination_initiated = false;

    #[cfg(unix)]
    {
        let pid = child.id();
        // The supervisor is started in its own process group, so a negative
        // PID targets only that owned group and its descendant daemon.
        if let Err(error) = signal_supervisor_group(pid, SIGTERM) {
            utility_errors.push(error.to_string());
        } else {
            let deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(error) => {
                        utility_errors.push(format!("failed to inspect supervisor: {error}"));
                        break;
                    }
                }
            }
        }
        if child_still_running(child) {
            if let Err(error) = signal_supervisor_group(pid, SIGKILL) {
                utility_errors.push(error.to_string());
            } else {
                forceful_termination_initiated = true;
            }
        }
    }
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        for force in [false, true] {
            let mut command = Command::new("taskkill");
            command.args(["/PID", &pid, "/T"]);
            if force {
                command.arg("/F");
            }
            match command.status() {
                Ok(status) if status.success() => {
                    if force {
                        forceful_termination_initiated = true;
                    }
                    if !force {
                        let deadline = Instant::now() + Duration::from_millis(250);
                        while Instant::now() < deadline {
                            match child.try_wait() {
                                Ok(Some(_)) => break,
                                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                                Err(error) => {
                                    utility_errors
                                        .push(format!("failed to inspect supervisor: {error}"));
                                    break;
                                }
                            }
                        }
                    }
                }
                Ok(status) => utility_errors.push(format!("taskkill exited with status {status}")),
                Err(error) => utility_errors.push(format!("taskkill failed: {error}")),
            }
            if !child_still_running(child) {
                break;
            }
        }
    }

    if child_still_running(child) {
        match child.kill() {
            Ok(()) => forceful_termination_initiated = true,
            Err(error) => utility_errors.push(format!("fallback child kill failed: {error}")),
        }
    }
    let child_running = child_still_running(child);
    if !child_running || forceful_termination_initiated {
        if let Err(error) = child.wait() {
            utility_errors.push(format!("failed to reap supervisor: {error}"));
        }
    }
    if child_still_running(child) {
        utility_errors.push("supervisor remained running after cleanup".to_string());
    }
    if utility_errors.is_empty() {
        Ok(())
    } else {
        Err(std::io::Error::other(utility_errors.join("; ")))
    }
}

fn child_still_running(child: &mut Child) -> bool {
    !matches!(child.try_wait(), Ok(Some(_)))
}

#[cfg(unix)]
fn signal_supervisor_group(pid: u32, signal: i32) -> std::io::Result<()> {
    let pid = i32::try_from(pid)
        .map_err(|_| std::io::Error::other("supervisor PID does not fit in pid_t"))?;
    let result = unsafe { kill(-pid, signal) };
    let error = std::io::Error::last_os_error();
    if result == 0 || error.raw_os_error() == Some(ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn parse_readiness_line(line: &str) -> Result<(), String> {
    let line = line.trim_end();
    if line == "READY" {
        return Ok(());
    }
    if let Some(message) = line.strip_prefix("ERROR ")
        && !message.trim().is_empty()
    {
        return Err(message.trim().to_string());
    }
    if line.is_empty() {
        Err("desktop supervisor returned an empty readiness response".to_string())
    } else {
        Err(format!(
            "desktop supervisor returned an invalid readiness response: {line}"
        ))
    }
}

fn desktop_daemon_command(binary: &Path, port: u16) -> Command {
    let mut cmd = Command::new(binary);
    cmd.arg("service")
        .arg("run-desktop-daemon")
        .arg("--port")
        .arg(port.to_string());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_command_targets_hidden_supervisor_and_port() {
        let command = desktop_daemon_command(Path::new("/tmp/zeroclaw"), 42617);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["service", "run-desktop-daemon", "--port", "42617"]);
    }

    #[test]
    fn readiness_line_parser_accepts_ready() {
        assert_eq!(parse_readiness_line("READY\n"), Ok(()));
    }

    #[test]
    fn readiness_line_parser_surfaces_error_detail() {
        assert_eq!(
            parse_readiness_line("ERROR could not open desktop log\n"),
            Err("could not open desktop log".to_string())
        );
    }

    #[test]
    fn readiness_line_parser_rejects_invalid_and_empty_lines() {
        let invalid = parse_readiness_line("NOT_READY\n").expect_err("invalid line");
        assert!(invalid.contains("NOT_READY"));
        assert_eq!(
            parse_readiness_line("\n"),
            Err("desktop supervisor returned an empty readiness response".to_string())
        );
    }

    #[test]
    fn readiness_frame_rejects_oversized_and_unterminated_input() {
        let oversized = vec![b'x'; READINESS_FRAME_MAX_BYTES + 1];
        let error = read_readiness_frame(oversized.as_slice()).expect_err("oversized frame");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeded"));

        let error = read_readiness_frame(b"READY".as_slice()).expect_err("unterminated frame");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(error.to_string().contains("newline"));
    }

    #[test]
    fn cleanup_failure_is_attached_to_startup_error() {
        let startup = std::io::Error::new(std::io::ErrorKind::TimedOut, "readiness timed out");
        let cleanup = std::io::Error::other("process tree still running");
        let error = attach_cleanup_error(startup, Err(cleanup));
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("readiness timed out"));
        assert!(error.to_string().contains("supervisor cleanup failed"));
        assert!(error.to_string().contains("process tree still running"));
    }
}
