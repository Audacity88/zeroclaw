//! Resident-memory sampling for one owned native command and its visible descendants.

use std::collections::{HashMap, HashSet};
use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
const BYTES_PER_MB: u64 = 1024 * 1024;

/// The caller owns Unix process-group cleanup, including cancellation and monitor failures.
/// Windows supervision owns a Job Object; disabled supervision keeps ordinary child waiting.
pub(super) struct ManagedChild {
    #[cfg(not(windows))]
    child: Child,
    #[cfg(windows)]
    child: WindowsChild,
}

#[cfg(windows)]
enum WindowsChild {
    Ordinary(Child),
    Job(Box<dyn process_wrap::tokio::ChildWrapper>),
}

#[derive(Debug)]
pub(super) enum MemoryWaitError {
    Io(io::Error),
    Exceeded { limit_mb: u64, rss_bytes: u64 },
    Unavailable { diagnostic: String },
}

impl ManagedChild {
    pub(super) fn spawn(mut command: Command, memory_mb: u64) -> io::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(windows)]
        {
            use process_wrap::tokio::{CommandWrap, JobObject, KillOnDrop};
            let child = if memory_mb == 0 {
                WindowsChild::Ordinary(command.spawn()?)
            } else {
                let mut command = CommandWrap::from(command);
                command
                    .wrap(KillOnDrop)
                    .wrap(crate::service::WindowsSpawnFailureGuard)
                    .wrap(JobObject);
                WindowsChild::Job(command.spawn()?)
            };
            Ok(Self { child })
        }
        #[cfg(not(windows))]
        {
            let _ = memory_mb;
            Ok(Self {
                child: command.spawn()?,
            })
        }
    }

    pub(super) fn id(&self) -> Option<u32> {
        #[cfg(not(windows))]
        {
            self.child.id()
        }
        #[cfg(windows)]
        match &self.child {
            WindowsChild::Ordinary(child) => child.id(),
            WindowsChild::Job(child) => child.id(),
        }
    }

    pub(super) fn stdout(&mut self) -> &mut Option<ChildStdout> {
        #[cfg(not(windows))]
        {
            &mut self.child.stdout
        }
        #[cfg(windows)]
        match &mut self.child {
            WindowsChild::Ordinary(child) => &mut child.stdout,
            WindowsChild::Job(child) => child.stdout(),
        }
    }

    pub(super) fn stderr(&mut self) -> &mut Option<ChildStderr> {
        #[cfg(not(windows))]
        {
            &mut self.child.stderr
        }
        #[cfg(windows)]
        match &mut self.child {
            WindowsChild::Ordinary(child) => &mut child.stderr,
            WindowsChild::Job(child) => child.stderr(),
        }
    }

    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        #[cfg(not(windows))]
        {
            self.child.start_kill()
        }
        #[cfg(windows)]
        match &mut self.child {
            WindowsChild::Ordinary(child) => child.start_kill(),
            WindowsChild::Job(child) => child.start_kill(),
        }
    }

    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(not(windows))]
        {
            self.child.wait().await
        }
        #[cfg(windows)]
        match &mut self.child {
            WindowsChild::Ordinary(child) => child.wait().await,
            // The watchdog covers the root lifetime on every platform. Keep
            // the Job Object for containment, but do not use its wait-for-all
            // completion-port wrapper. Closing the owned job stops leftovers.
            WindowsChild::Job(child) => child.inner_mut().wait().await,
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(not(windows))]
        {
            self.child.try_wait()
        }
        #[cfg(windows)]
        match &mut self.child {
            WindowsChild::Ordinary(child) => child.try_wait(),
            WindowsChild::Job(child) => child.inner_mut().try_wait(),
        }
    }

    /// Monitor failure leaves the root unreaped so the caller can kill its owned group/job
    /// before reaping, then report the original memory failure regardless of cleanup status.
    pub(super) async fn wait_with_memory(
        &mut self,
        memory_mb: u64,
    ) -> Result<ExitStatus, MemoryWaitError> {
        if memory_mb == 0 {
            return self.wait().await.map_err(MemoryWaitError::Io);
        }
        let Some(root) = self.id().map(Pid::from_u32) else {
            return self.wait().await.map_err(MemoryWaitError::Io);
        };
        #[cfg(windows)]
        if matches!(self.child, WindowsChild::Ordinary(_)) {
            return Err(MemoryWaitError::Unavailable {
                diagnostic: "memory supervision requires an owned Job Object".into(),
            });
        }
        let limit_bytes = memory_mb.saturating_mul(BYTES_PER_MB);
        let cancellation = SampleCancellation(Arc::new(AtomicBool::new(false)));
        let mut root_start_time = None;
        loop {
            if self.try_wait().map_err(MemoryWaitError::Io)?.is_some() {
                return self.wait().await.map_err(MemoryWaitError::Io);
            }
            let cancelled = Arc::clone(&cancellation.0);
            // Do not poll/reap the root while sampling. On Unix its unreaped PID cannot
            // be recycled, including when it exits during this blocking observation.
            let sample = tokio::task::spawn_blocking(move || {
                sample_memory(root, root_start_time, &cancelled)
            })
            .await;
            // A missing/zombie root is normal completion when the owned wait confirms it.
            if self.try_wait().map_err(MemoryWaitError::Io)?.is_some() {
                return self.wait().await.map_err(MemoryWaitError::Io);
            }
            let (rss_bytes, start_time) = sample
                .map_err(|error| MemoryWaitError::Unavailable {
                    diagnostic: format!("memory sampler failed: {error}"),
                })?
                .map_err(|diagnostic| MemoryWaitError::Unavailable {
                    diagnostic: diagnostic.into(),
                })?;
            root_start_time = Some(start_time);
            if rss_bytes > limit_bytes {
                return Err(MemoryWaitError::Exceeded {
                    limit_mb: memory_mb,
                    rss_bytes,
                });
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
    }
}

struct SampleCancellation(Arc<AtomicBool>);

impl Drop for SampleCancellation {
    fn drop(&mut self) {
        // spawn_blocking cannot abort an in-progress sysinfo call. It may finish its
        // current observation, but cancellation prevents further phases or samples.
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ProcessIdentity {
    parent: Option<Pid>,
    start_time: u64,
}

fn sample_memory(
    root: Pid,
    root_start_time: Option<u64>,
    cancelled: &AtomicBool,
) -> Result<(u64, u64), &'static str> {
    if cancelled.load(Ordering::Acquire) {
        return Err("memory observation cancelled");
    }
    // A fresh snapshot prevents a failed memory refresh from reusing stale RSS.
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    let root_process = system.process(root).ok_or("owned root is not observable")?;
    let start_time = root_process.start_time();
    if root_start_time.is_some_and(|expected| expected != start_time) {
        return Err("owned root identity changed");
    }
    let mut children = HashMap::<Pid, Vec<Pid>>::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    let mut discovered = HashSet::from([root]);
    let mut pending = vec![root];
    while let Some(pid) = pending.pop() {
        if let Some(descendants) = children.get(&pid) {
            for child in descendants {
                if discovered.insert(*child) {
                    pending.push(*child);
                }
            }
        }
    }
    let identities: HashMap<_, _> = discovered
        .iter()
        .filter_map(|pid| {
            system.process(*pid).map(|process| {
                (
                    *pid,
                    ProcessIdentity {
                        parent: process.parent(),
                        start_time: process.start_time(),
                    },
                )
            })
        })
        .collect();
    if cancelled.load(Ordering::Acquire) {
        return Err("memory observation cancelled");
    }
    let pids: Vec<_> = discovered.into_iter().collect();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing().without_tasks().with_memory(),
    );
    if cancelled.load(Ordering::Acquire) {
        return Err("memory observation cancelled");
    }
    let root_process = system.process(root).ok_or("owned root is not observable")?;
    if root_process.start_time() != start_time {
        return Err("owned root identity changed");
    }
    if root_process.memory() == 0 {
        return Err("owned root resident memory is not observable");
    }
    let mut rss_bytes = 0_u64;
    let mut pending = vec![root];
    let mut counted = HashSet::new();
    while let Some(pid) = pending.pop() {
        if !counted.insert(pid) {
            continue;
        }
        let Some(identity) = identities.get(&pid) else {
            continue;
        };
        let Some(process) = system.process(pid) else {
            continue;
        };
        let refreshed = ProcessIdentity {
            parent: process.parent(),
            start_time: process.start_time(),
        };
        // Never charge a process that replaced a descendant between the two reads.
        if refreshed != *identity {
            if pid == root {
                return Err("owned root identity changed");
            }
            continue;
        }
        if process.memory() == 0 && process.status() != ProcessStatus::Zombie {
            return Err("descendant resident memory is not observable");
        }
        rss_bytes = rss_bytes.saturating_add(process.memory());
        // An unchanged descendant is eligible only through unchanged, visible parents.
        if let Some(descendants) = children.get(&pid) {
            pending.extend(descendants);
        }
    }
    Ok((rss_bytes, start_time))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    const FIXTURE_ENV: &str = "ZEROCLAW_RESIDENT_MEMORY_FIXTURE";
    const FIXTURE_TEST: &str = "tools::subprocess_memory::tests::memory_fixture";
    // Bound aggregate test-host pressure even when the surrounding suite runs in parallel.
    static FIXTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn fixture_command(case: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", FIXTURE_TEST, "--nocapture", "--test-threads=1"])
            .env(FIXTURE_ENV, case)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        command
    }

    #[cfg(unix)]
    struct OwnedFixtureGroup(Option<u32>);

    #[cfg(unix)]
    impl Drop for OwnedFixtureGroup {
        fn drop(&mut self) {
            if let Some(pid) = self.0.take() {
                // SAFETY: this test owns the still-unreaped process-group leader.
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
        }
    }

    async fn run_fixture(case: &str, memory_mb: u64) -> Result<ExitStatus, MemoryWaitError> {
        let _fixture_lock = FIXTURE_LOCK.lock().await;
        let mut child = ManagedChild::spawn(fixture_command(case), memory_mb).unwrap();
        #[cfg(unix)]
        let mut group = OwnedFixtureGroup(child.id());
        let stdout = child.stdout().take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if line.contains("MEMORY_FIXTURE_READY") {
                    return;
                }
            }
            panic!("memory fixture did not start");
        })
        .await
        .expect("memory fixture startup timed out");
        let result =
            tokio::time::timeout(Duration::from_secs(10), child.wait_with_memory(memory_mb)).await;
        if matches!(result, Ok(Ok(_))) {
            #[cfg(unix)]
            {
                group.0 = None;
            }
        } else {
            #[cfg(unix)]
            drop(group);
            let _ = child.start_kill();
            tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .expect("owned memory fixture cleanup timed out")
                .unwrap();
        }
        result.expect("memory watchdog fixture timed out")
    }

    #[tokio::test]
    async fn touched_resident_allocation_exceeds_threshold() {
        let result = run_fixture("resident", 128).await;
        assert!(matches!(
            result,
            Err(MemoryWaitError::Exceeded {
                limit_mb: 128,
                rss_bytes,
            }) if rss_bytes > 128 * BYTES_PER_MB
        ));
    }

    #[tokio::test]
    async fn descendant_resident_allocation_counts_toward_threshold() {
        let result = run_fixture("descendant", 224).await;
        assert!(matches!(
            result,
            Err(MemoryWaitError::Exceeded {
                limit_mb: 224,
                rss_bytes,
            }) if rss_bytes > 224 * BYTES_PER_MB
        ));
    }

    #[cfg(all(unix, target_pointer_width = "64"))]
    #[tokio::test]
    async fn virtual_reservation_does_not_exceed_resident_threshold() {
        assert!(run_fixture("virtual", 128).await.unwrap().success());
    }

    #[tokio::test]
    async fn zero_threshold_preserves_ordinary_wait() {
        assert!(run_fixture("resident", 0).await.unwrap().success());
    }

    #[tokio::test]
    async fn ordinary_completion_under_budget() {
        assert!(run_fixture("ordinary", 128).await.unwrap().success());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn root_exit_closes_owned_job_descendants() {
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };

        let mut child = ManagedChild::spawn(fixture_command("root-first"), 512).unwrap();
        let mut lines = BufReader::new(child.stdout().take().unwrap()).lines();
        let pid = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some(pid) = line.strip_prefix("MEMORY_FIXTURE_CHILD=") {
                    return pid.parse::<u32>().unwrap();
                }
            }
            panic!("owned descendant did not start");
        })
        .await
        .unwrap();
        // SAFETY: the PID came from the child this test spawned in its owned job.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.unwrap();
        let result =
            tokio::time::timeout(Duration::from_secs(3), child.wait_with_memory(512)).await;
        drop(child);
        // SAFETY: wait on and close only the handle opened above.
        let exited = unsafe { WaitForSingleObject(handle, 5_000) };
        unsafe { CloseHandle(handle) }.unwrap();
        assert!(result.unwrap().unwrap().success());
        assert_eq!(
            exited, WAIT_OBJECT_0,
            "owned job descendant survived root exit"
        );
    }

    fn hold_resident_memory(memory_mb: usize) {
        let mut allocation = vec![0_u8; memory_mb * BYTES_PER_MB as usize];
        for page in allocation.chunks_mut(4096) {
            // SAFETY: every chunk is nonempty and belongs to the live allocation.
            unsafe { std::ptr::write_volatile(page.as_mut_ptr(), 1) };
        }
        println!("MEMORY_FIXTURE_READY");
        std::thread::sleep(Duration::from_secs(2));
        std::hint::black_box(allocation);
    }

    #[test]
    fn memory_fixture() {
        let Ok(case) = std::env::var(FIXTURE_ENV) else {
            return;
        };
        match case.as_str() {
            #[cfg(windows)]
            "root-first" => {
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", FIXTURE_TEST, "--nocapture", "--test-threads=1"])
                    .env(FIXTURE_ENV, "lingering")
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap();
                println!("MEMORY_FIXTURE_CHILD={}", child.id());
            }
            #[cfg(windows)]
            "lingering" => std::thread::sleep(Duration::from_secs(30)),
            "resident" => hold_resident_memory(256),
            "small-resident" => hold_resident_memory(128),
            "descendant" => {
                // Each descendant touches less than the aggregate threshold. The root
                // remains alive and small while both owned descendants hold their pages.
                let mut descendants = Vec::new();
                for _ in 0..2 {
                    descendants.push(
                        std::process::Command::new(std::env::current_exe().unwrap())
                            .args(["--exact", FIXTURE_TEST, "--nocapture", "--test-threads=1"])
                            .env(FIXTURE_ENV, "small-resident")
                            .spawn()
                            .unwrap(),
                    );
                }
                for mut descendant in descendants {
                    assert!(descendant.wait().unwrap().success());
                }
            }
            #[cfg(all(unix, target_pointer_width = "64"))]
            "virtual" => {
                let len = 2 * 1024 * BYTES_PER_MB as usize;
                // SAFETY: reserve anonymous inaccessible pages without touching them.
                let reservation = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        len,
                        libc::PROT_NONE,
                        libc::MAP_PRIVATE | libc::MAP_ANON,
                        -1,
                        0,
                    )
                };
                assert_ne!(reservation, libc::MAP_FAILED);
                println!("MEMORY_FIXTURE_READY");
                std::thread::sleep(Duration::from_millis(300));
                // SAFETY: unmap exactly the range successfully reserved above.
                assert_eq!(unsafe { libc::munmap(reservation, len) }, 0);
            }
            "ordinary" => {
                println!("MEMORY_FIXTURE_READY");
                std::thread::sleep(Duration::from_millis(300));
            }
            _ => panic!("unknown owned memory fixture"),
        }
    }
}
