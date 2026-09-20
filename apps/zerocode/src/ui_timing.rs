//! Opt-in, content-free diagnostics for local interaction stalls.
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PHASES: [&str; 10] = [
    "init",
    "tick",
    "status_write",
    "render",
    "terminal_output",
    "reconnect",
    "poll",
    "read",
    "dispatch",
    "idle_tick",
];
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum Phase {
    Init,
    Tick,
    StatusWrite,
    Render,
    TerminalOutput,
    Reconnect,
    Poll,
    Read,
    Dispatch,
    IdleTick,
}

struct State {
    start: Instant,
    // One app-loop producer publishes phase and entry time together.
    phase: AtomicU64,
    maxima: [AtomicU64; PHASES.len()],
    loops: AtomicU64,
    inputs: AtomicU64,
    last_input: AtomicU64,
    dispatches: AtomicU64,
    frames: AtomicU64,
    last_frame: AtomicU64,
    stop: AtomicBool,
}

impl State {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            phase: AtomicU64::new(Phase::Init as u64),
            maxima: std::array::from_fn(|_| AtomicU64::new(0)),
            loops: AtomicU64::new(0),
            inputs: AtomicU64::new(0),
            last_input: AtomicU64::new(0),
            dispatches: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            last_frame: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }
    }

    fn now_us(&self) -> u64 {
        self.start
            .elapsed()
            .as_micros()
            .min((u64::MAX >> 8) as u128) as u64
    }

    fn mark_at(&self, phase: Phase, now: u64) {
        let previous = self.phase.swap((now << 8) | phase as u64, Relaxed);
        self.maxima[(previous & 255) as usize]
            .fetch_max(now.saturating_sub(previous >> 8), Relaxed);
        if matches!(phase, Phase::Tick) {
            self.loops.fetch_add(1, Relaxed);
        }
        if matches!(phase, Phase::Dispatch) {
            self.dispatches.fetch_add(1, Relaxed);
        }
    }

    fn row(&self) -> String {
        let phase = self.phase.load(Relaxed);
        let now = self.now_us();
        let index = (phase & 255) as usize;
        let age = now.saturating_sub(phase >> 8);
        let mut row = format!(
            "{},{now},{},{age},{},{},{},{},{},{},{}",
            unix_ms(),
            PHASES[index],
            self.loops.load(Relaxed),
            self.inputs.load(Relaxed),
            self.last_input.load(Relaxed),
            self.dispatches.load(Relaxed),
            self.frames.load(Relaxed),
            self.last_frame.load(Relaxed),
            self.stop.load(Relaxed),
        );
        for (i, maximum) in self.maxima.iter().enumerate() {
            let elapsed = maximum
                .swap(0, Relaxed)
                .max(if i == index { age } else { 0 });
            // Formatting into String cannot fail.
            let _ = write!(row, ",{elapsed}");
        }
        row.push('\n');
        row
    }
}

pub(crate) struct TimingTrace(Option<(Arc<State>, JoinHandle<()>)>);

impl TimingTrace {
    pub(crate) fn from_env() -> io::Result<Self> {
        if std::env::var_os("ZEROCODE_TIMING").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return Ok(Self(None));
        }
        let path = std::env::temp_dir().join(format!(
            "zerocode-timing-{}-{}.csv",
            std::process::id(),
            unix_ms()
        ));
        Self::start(&path)
    }

    fn start(path: &std::path::Path) -> io::Result<Self> {
        let path = path.to_owned();
        let state = Arc::new(State::new());
        let worker_state = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name("zerocode-timing".into())
            .spawn(move || {
                if let Err(error) = capture(&worker_state, &path) {
                    eprintln!("zerocode timing capture failed: {error}");
                }
            })?;
        Ok(Self(Some((state, worker))))
    }

    pub(crate) fn mark(&self, phase: Phase) {
        if let Some((state, _)) = &self.0 {
            state.mark_at(phase, state.now_us());
        }
    }

    pub(crate) fn input_received(&self) {
        if let Some((state, _)) = &self.0 {
            state.last_input.store(state.now_us(), Relaxed);
            state.inputs.fetch_add(1, Relaxed);
        }
    }

    pub(crate) fn frame_completed(&self) {
        if let Some((state, _)) = &self.0 {
            state.last_frame.store(state.now_us(), Relaxed);
            state.frames.fetch_add(1, Relaxed);
        }
    }
}

impl Drop for TimingTrace {
    fn drop(&mut self) {
        if let Some((state, worker)) = &self.0 {
            state.stop.store(true, Relaxed);
            worker.thread().unpark();
            // Never join a writer that could be waiting on the filesystem.
        }
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn write_bounded(writer: &mut impl Write, row: &str, remaining: &mut usize) -> io::Result<bool> {
    const END: &str = "# capture_size_limit_reached\n";
    if row.len() + END.len() > *remaining {
        if END.len() <= *remaining {
            writer.write_all(END.as_bytes())?;
            *remaining -= END.len();
        }
        return Ok(false);
    }
    writer.write_all(row.as_bytes())?;
    *remaining -= row.len();
    Ok(true)
}

fn open_capture(path: &std::path::Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn capture(state: &State, path: &std::path::Path) -> io::Result<()> {
    let mut file = open_capture(path)?;
    let mut header = String::from(
        "wall_ms,elapsed_us,phase,phase_age_us,loops,inputs,last_input_us,dispatches,frames,last_frame_us,stopping",
    );
    for name in PHASES {
        let _ = write!(header, ",max_{name}_us");
    }
    header.push('\n');
    file.write_all(header.as_bytes())?;
    let mut remaining = MAX_BYTES - header.len();
    loop {
        if !write_bounded(&mut file, &state.row(), &mut remaining)? || state.stop.load(Relaxed) {
            return Ok(());
        }
        thread::park_timeout(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_timing_writer_observes_a_stalled_producer_and_uses_private_new_file() {
        let path = std::env::temp_dir().join(format!(
            "zerocode-timing-test-{}-{}.csv",
            std::process::id(),
            unix_ms()
        ));
        let mut trace = TimingTrace::start(&path).unwrap();
        trace.input_received();
        trace.frame_completed();
        trace.mark(Phase::Dispatch);
        let deadline = Instant::now() + Duration::from_secs(5);
        let captured = loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let found = text.lines().skip(1).any(|row| {
                let cells: Vec<_> = row.split(',').collect();
                cells.len() == 21
                    && cells[2] == "dispatch"
                    && cells[3].parse::<u64>().unwrap() >= 1_000_000
                    && cells[5] == "1"
                    && cells[7] == "1"
                    && cells[8] == "1"
            });
            if found || Instant::now() >= deadline {
                break found;
            }
            thread::sleep(Duration::from_millis(25));
        };
        let (state, worker) = trace.0.as_ref().unwrap();
        state.stop.store(true, Relaxed);
        worker.thread().unpark();
        // Joining is test-only; the UI never waits for the writer.
        let (_, worker) = trace.0.take().unwrap();
        worker.join().unwrap();
        assert!(
            open_capture(&path).is_err(),
            "never overwrite an existing capture"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_file(path).unwrap();
        assert!(
            captured,
            "watchdog must progress while producer stays in dispatch"
        );
    }

    #[test]
    fn ui_timing_records_completed_and_current_phases() {
        let state = State::new();
        state.mark_at(Phase::Tick, 10);
        state.mark_at(Phase::Dispatch, 30);
        state.mark_at(Phase::Render, 90);
        assert_eq!(state.phase.load(Relaxed), (90 << 8) | Phase::Render as u64);
        assert_eq!(state.maxima[Phase::Tick as usize].load(Relaxed), 20);
        assert_eq!(state.maxima[Phase::Dispatch as usize].load(Relaxed), 60);
        assert_eq!(state.loops.load(Relaxed), 1);
        assert_eq!(state.dispatches.load(Relaxed), 1);
        assert!(state.row().contains(",render,"));
        assert_eq!(state.maxima[Phase::Dispatch as usize].load(Relaxed), 0);
    }

    #[test]
    fn ui_timing_bounds_output_and_propagates_write_failure() {
        let mut output = Vec::new();
        let mut remaining = 60;
        assert!(write_bounded(&mut output, "numeric,row\n", &mut remaining).unwrap());
        assert!(!write_bounded(&mut output, &"x".repeat(60), &mut remaining).unwrap());
        assert!(output.ends_with(b"# capture_size_limit_reached\n"));
        assert!(output.len() <= 60);
        assert!(write_bounded(&mut io::sink(), "row\n", &mut 0).is_ok());
        struct Failed;
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::StorageFull.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(write_bounded(&mut Failed, "row\n", &mut 60).is_err());
    }
}
