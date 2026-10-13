//! Bounded Git execution for optional OpenSkills refreshes.
use process_wrap::std::CommandWrap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(super) const EXECUTION_TIMEOUT: Duration = Duration::from_secs(2);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Success,
    Failed,
    CleanupPending,
}

impl From<bool> for Outcome {
    fn from(success: bool) -> Self {
        if success { Self::Success } else { Self::Failed }
    }
}

#[derive(Debug)]
pub(super) struct Failure {
    pub outcome: Outcome,
    message: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self {
            outcome: Outcome::Failed,
            message: error.to_string(),
        }
    }
}

fn git() -> Command {
    let mut command = Command::new("git");
    // Do not launch detached maintenance from this request-scoped operation.
    command.args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"]);
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

pub(super) fn pull(repo: &Path) -> Result<(), Failure> {
    run(
        git().arg("-C").arg(repo).args(["pull", "--ff-only"]),
        EXECUTION_TIMEOUT,
    )
}

pub(super) fn clone_repo(repo: &Path, url: &str) -> Result<(), Failure> {
    let parent = repo
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".open-skills-")
        .tempdir_in(parent)?;
    let checkout = staging.path().join("checkout");
    if let Err(error) = run(
        git()
            .args(["clone", "--depth", "1", "--"])
            .arg(url)
            .arg(&checkout),
        EXECUTION_TIMEOUT,
    ) {
        if error.outcome == Outcome::CleanupPending {
            // A surviving writer must not race TempDir's recursive deletion.
            let _retained = staging.keep();
        }
        return Err(error);
    }
    publish(&checkout, repo)?;
    Ok(())
}

fn run(command: &mut Command, timeout: Duration) -> Result<(), Failure> {
    // A file cannot hold the caller waiting for pipe EOF from a Git descendant.
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr.try_clone()?);
    let command = std::mem::replace(command, Command::new("git"));
    let mut command = CommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(windows_job::WindowsJob::new()?);
    let deadline = Instant::now() + timeout;
    let mut child = command.spawn().map_err(|error| {
        let mut failure = Failure::from(error);
        if cfg!(windows) {
            // Setup can fail after resuming a child; do not assume the job has drained.
            failure.outcome = Outcome::CleanupPending;
        }
        failure
    })?;
    let pid = child.id();
    let result = loop {
        // Preserve the wrapper's whole-job wait for cleanup, even if the leader exits first.
        match child.inner_mut().try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error),
            Ok(None) if Instant::now() >= deadline => {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "OpenSkills Git exceeded its execution deadline",
                ));
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
        }
    };
    let cleanup_deadline = Instant::now() + CLEANUP_TIMEOUT;
    let killed = child.start_kill();
    #[cfg(unix)]
    let killed = match killed {
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        other => other,
    };
    // Even an OS-level wait must not monopolize the session request indefinitely.
    // On an exceptional cleanup timeout this worker keeps ownership of the direct child.
    let (tx, rx) = mpsc::sync_channel(1);
    let reaper = std::thread::Builder::new()
        .name("open-skills-reap".into())
        .spawn(move || {
            let waited = child.wait();
            let _ = tx.send(waited);
        });
    let reaped = if reaper.is_ok() {
        rx.recv_timeout(cleanup_deadline.saturating_duration_since(Instant::now()))
            .map_err(io::Error::other)
            .and_then(|result| result)
    } else {
        Err(io::Error::other("could not start Git cleanup worker"))
    };
    let gone = reaped.is_ok() && group_gone(pid, cleanup_deadline);
    if killed.is_err() || !gone {
        return Err(Failure {
            outcome: Outcome::CleanupPending,
            message: format!(
                "OpenSkills Git cleanup could not be confirmed (kill: {killed:?}, reap: {reaped:?}, group gone: {gone}); skipping this repository for the rest of this process"
            ),
        });
    }
    let status = result?;
    if status.success() {
        return Ok(());
    }
    stderr.seek(SeekFrom::Start(0))?;
    let mut diagnostic = Vec::new();
    stderr.take(8192).read_to_end(&mut diagnostic)?;
    Err(Failure {
        outcome: Outcome::Failed,
        message: format!(
            "OpenSkills Git exited with {status}: {}",
            String::from_utf8_lossy(&diagnostic)
        ),
    })
}

#[cfg(unix)]
fn group_gone(pid: u32, deadline: Instant) -> bool {
    loop {
        // The group was created with the direct child's PID; signal zero only probes it.
        if unsafe { libc::kill(-(pid as libc::pid_t), 0) } == -1 {
            return io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(not(unix))]
fn group_gone(_pid: u32, _deadline: Instant) -> bool {
    // WindowsJobChild::wait above confirms ActiveProcesses is zero.
    true
}

#[cfg(unix)]
fn publish(staged: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let staged = CString::new(staged.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            staged.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let result =
        unsafe { libc::renamex_np(staged.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace clone publication is unavailable",
    ));
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn publish(staged: &Path, destination: &Path) -> io::Result<()> {
    // Windows directory rename fails when the destination already exists.
    std::fs::rename(staged, destination)
}

#[cfg(windows)]
mod windows_job {
    use super::*;
    use process_wrap::std::{ChildWrapper, CommandWrapper};
    use std::os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    };
    use std::sync::Arc;
    use windows::Win32::{
        Foundation::{ERROR_NO_MORE_FILES, HANDLE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
                QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
            },
            Threading::{CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    #[derive(Debug)]
    struct Job(OwnedHandle);

    impl Job {
        fn handle(&self) -> HANDLE {
            HANDLE(self.0.as_raw_handle())
        }
        fn kill(&self) -> io::Result<()> {
            unsafe { TerminateJobObject(self.handle(), 1) }.map_err(io::Error::other)
        }
        fn empty(&self) -> io::Result<bool> {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            unsafe {
                QueryInformationJobObject(
                    Some(self.handle()),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of_val(&info) as u32,
                    None,
                )
            }
            .map_err(io::Error::other)?;
            Ok(info.ActiveProcesses == 0)
        }
    }

    #[derive(Debug)]
    pub(super) struct WindowsJob(Arc<Job>);

    impl WindowsJob {
        pub(super) fn new() -> io::Result<Self> {
            let raw = unsafe { CreateJobObjectW(None, None) }.map_err(io::Error::other)?;
            let job = Job(unsafe { OwnedHandle::from_raw_handle(raw.0) });
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            unsafe {
                SetInformationJobObject(
                    job.handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            }
            .map_err(io::Error::other)?;
            Ok(Self(Arc::new(job)))
        }
    }

    impl CommandWrapper for WindowsJob {
        fn pre_spawn(&mut self, command: &mut Command, _core: &CommandWrap) -> io::Result<()> {
            command.creation_flags(CREATE_SUSPENDED.0);
            Ok(())
        }

        fn wrap_child(
            &mut self,
            child: Box<dyn ChildWrapper>,
            _core: &CommandWrap,
        ) -> io::Result<Box<dyn ChildWrapper>> {
            // Own both child and job before any fallible assignment/resume operation.
            let guarded = WindowsJobChild {
                child: Some(child),
                job: Arc::clone(&self.0),
            };
            let handle = HANDLE(guarded.inner().inner_child().as_raw_handle());
            unsafe { AssignProcessToJobObject(self.0.handle(), handle) }
                .map_err(io::Error::other)?;
            resume(guarded.id())?;
            Ok(Box::new(guarded))
        }
    }

    fn resume(pid: u32) -> io::Result<()> {
        let raw =
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }.map_err(io::Error::other)?;
        let snapshot = unsafe { OwnedHandle::from_raw_handle(raw.0) };
        let handle = HANDLE(snapshot.as_raw_handle());
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        unsafe { Thread32First(handle, &mut entry) }.map_err(io::Error::other)?;
        let mut resumed = false;
        loop {
            if entry.th32OwnerProcessID == pid {
                let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
                    .map_err(io::Error::other)?;
                let thread = unsafe { OwnedHandle::from_raw_handle(raw.0) };
                if unsafe { ResumeThread(HANDLE(thread.as_raw_handle())) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed = true;
            }
            match unsafe { Thread32Next(handle, &mut entry) } {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        if resumed {
            Ok(())
        } else {
            Err(io::Error::other("Git initial thread was not found"))
        }
    }

    #[derive(Debug)]
    struct WindowsJobChild {
        child: Option<Box<dyn ChildWrapper>>,
        job: Arc<Job>,
    }

    impl ChildWrapper for WindowsJobChild {
        fn inner(&self) -> &dyn ChildWrapper {
            self.child
                .as_deref()
                .expect("guard owns child until consumed")
        }
        fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
            self.child
                .as_deref_mut()
                .expect("guard owns child until consumed")
        }
        fn into_inner(mut self: Box<Self>) -> Box<dyn ChildWrapper> {
            self.child.take().expect("guard owns child until consumed")
        }
        fn start_kill(&mut self) -> io::Result<()> {
            self.job.kill()
        }
        fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
            let status = self.inner_mut().wait()?;
            while !self.job.empty()? {
                std::thread::sleep(POLL_INTERVAL);
            }
            Ok(status)
        }
    }

    impl Drop for WindowsJobChild {
        fn drop(&mut self) {
            let _ = self.job.kill();
            if let Some(mut child) = self.child.take() {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                let _ = child.start_kill();
                let _ = std::thread::Builder::new()
                    .name("open-skills-spawn-reap".into())
                    .spawn(move || child.wait());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn local_clone_succeeds_and_real_git_pull_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote");
        assert!(
            Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&remote)
                .status()
                .unwrap()
                .success()
        );
        let checkout = root.path().join("checkout");
        clone_repo(&checkout, remote.to_str().unwrap()).unwrap();
        assert!(checkout.join(".git").is_dir());
        for (key, value) in [
            ("protocol.ext.allow", "always"),
            ("remote.origin.url", "ext::sh -c sleep% 30"),
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&checkout)
                    .args(["config", "--local", key, value])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(checkout.join("local-skill"), "retained").unwrap();
        let started = Instant::now();
        let failure = pull(&checkout).unwrap_err();
        assert_eq!(failure.outcome, Outcome::Failed, "{failure}");
        assert!(failure.to_string().contains("deadline"), "{failure}");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(
            std::fs::read_to_string(checkout.join("local-skill")).unwrap(),
            "retained"
        );
    }

    #[test]
    fn normal_exit_and_failure_do_not_wait_for_pipe_eof() {
        run(
            Command::new("sh").args(["-c", "printf ok >&2"]),
            Duration::from_secs(2),
        )
        .unwrap();
        let error = run(
            Command::new("sh").args(["-c", "printf failure >&2; exit 7"]),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(error.outcome, Outcome::Failed);
        assert!(error.to_string().contains("failure"));
        assert!(
            run(
                &mut Command::new("/nonexistent/open-skills-git"),
                Duration::from_secs(2)
            )
            .is_err()
        );
    }

    #[test]
    fn hung_parent_and_descendant_are_stopped_before_return() {
        let temp = tempfile::tempdir().unwrap();
        let pids = temp.path().join("pids");
        let started = Instant::now();
        let error = run(
            Command::new("sh")
                .args(["-c", "sleep 30 & echo \"$$ $!\" > \"$1\"; wait", "sh"])
                .arg(&pids),
            Duration::from_millis(250),
        )
        .unwrap_err();
        assert_eq!(error.outcome, Outcome::Failed, "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
        for pid in std::fs::read_to_string(pids).unwrap().split_whitespace() {
            let pid: i32 = pid.parse().unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "process {pid} survived");
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        }
    }

    #[test]
    fn exited_leader_does_not_leave_a_background_writer() {
        let started = Instant::now();
        let result = run(
            Command::new("sh").args(["-c", "sleep 30 &"]),
            Duration::from_secs(2),
        );
        // Some kernels deny probing an orphan-only process group. Fail closed there.
        if let Err(error) = result {
            assert_eq!(error.outcome, Outcome::CleanupPending, "{error}");
        }
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn failed_clone_is_absent_and_publish_never_replaces_destination() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("skills");
        assert!(
            clone_repo(
                &destination,
                root.path().join("missing-repo").to_str().unwrap()
            )
            .is_err()
        );
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);

        let staged = root.path().join("staged");
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(staged.join("new"), "new").unwrap();
        std::fs::create_dir(&destination).unwrap();
        assert!(publish(&staged, &destination).is_err());
        assert!(staged.join("new").exists());
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        let winner = root.path().join("winner");
        publish(&staged, &winner).unwrap();
        assert!(winner.join("new").exists());
    }
}
