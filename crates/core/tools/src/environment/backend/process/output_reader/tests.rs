use super::*;
use crate::environment::backend::process::{JoinedProcessStatus, JoinedSessionOutput};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A liveness bound: a reader that never returns fails the test, not the lane.
const LIVENESS: Duration = Duration::from_secs(30);

struct Reader {
    thread: std::thread::JoinHandle<()>,
    stop: Arc<ReaderStop>,
    shared: Arc<Mutex<JoinedSessionShared>>,
    failed: Arc<AtomicBool>,
}

impl Reader {
    fn spawn(pipe: std::io::PipeReader, window: u64) -> Result<Self, Box<dyn std::error::Error>> {
        Self::spawn_with(pipe, window, ReaderStop::new())
    }

    fn spawn_with(
        pipe: std::io::PipeReader,
        window: u64,
        stop: ReaderStop,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let shared = Arc::new(Mutex::new(JoinedSessionShared {
            output: JoinedSessionOutput::default(),
            status: JoinedProcessStatus::Running,
            elapsed: None,
            terminal_changes: None,
            changes: tokio::sync::watch::channel(0).0,
            terminal: Arc::new(std::sync::Condvar::new()),
            released: false,
            spool: None,
            pending_spool: None,
            leftover: None,
            disowned: false,
        }));
        let stop = Arc::new(stop);
        let failed = Arc::new(AtomicBool::new(false));
        let limits = ExecutionEnvironmentLimits::new(
            window,
            window,
            window,
            window,
            Duration::from_secs(4),
            Duration::from_secs(4),
        )?;
        let thread = spawn_joined_output_reader(
            pipe,
            JoinedProcessOutputStream::Stdout,
            Arc::clone(&shared),
            Arc::clone(&failed),
            Arc::clone(&stop),
            limits,
        )?;
        Ok(Self {
            thread,
            stop,
            shared,
            failed,
        })
    }

    /// Joins the reader within the liveness bound and returns its retained bytes.
    fn join(self) -> Result<(Vec<u8>, JoinedSessionOutput), Box<dyn std::error::Error>> {
        if !super::super::join_thread_until(self.thread, Instant::now() + LIVENESS) {
            return Err("output reader did not return".into());
        }
        assert!(!self.failed.load(Ordering::Acquire));
        let output = std::mem::take(&mut lock(&self.shared).output);
        let retained = output
            .records
            .iter()
            .flat_map(|record| record.bytes.iter().copied())
            .collect();
        Ok((retained, output))
    }
}

#[test]
fn a_reader_returns_at_end_of_file_with_every_byte() -> TestResult {
    let (pipe, mut writer) = std::io::pipe()?;
    let reader = Reader::spawn(pipe, 4096)?;
    writer.write_all(b"first ")?;
    std::thread::sleep(Duration::from_millis(20));
    writer.write_all(b"second")?;
    drop(writer);
    let (retained, _) = reader.join()?;
    assert_eq!(retained, b"first second");
    Ok(())
}

/// The reader waits in `poll` with no timeout, so only the stop's wake can
/// end the wait while the writer stays open and silent.
#[test]
fn a_stop_wakes_a_reader_blocked_on_an_idle_pipe() -> TestResult {
    let (pipe, mut writer) = std::io::pipe()?;
    let reader = Reader::spawn(pipe, 4096)?;
    writer.write_all(b"before")?;
    std::thread::sleep(Duration::from_millis(50));
    reader.stop.stop();
    let (retained, _) = reader.join()?;
    assert_eq!(retained, b"before");
    // The writer was open throughout: the reader returned on the stop.
    drop(writer);
    Ok(())
}

/// Without room for a wake pipe the readers fall back to the 2 ms read
/// poll: output still arrives and a stop still ends them.
#[test]
fn a_reader_without_a_wake_polls_and_still_stops() -> TestResult {
    let (pipe, mut writer) = std::io::pipe()?;
    let unwoken = ReaderStop {
        stopped: AtomicBool::new(false),
        wake: None,
    };
    let reader = Reader::spawn_with(pipe, 4096, unwoken)?;
    writer.write_all(b"polled")?;
    std::thread::sleep(Duration::from_millis(50));
    reader.stop.stop();
    let (retained, _) = reader.join()?;
    assert_eq!(retained, b"polled");
    drop(writer);
    Ok(())
}

/// A descriptor `poll` fails on (or reports `NVAL` for) is read by the 2 ms
/// poll from then on, and `poll` is not asked again.
#[test]
fn a_poll_failure_falls_back_to_the_read_poll() -> TestResult {
    let stop = ReaderStop::new();
    assert!(stop.wake.is_some(), "the test process has room for a wake");
    let (pipe, _writer) = std::io::pipe()?;
    let mut pollable = true;
    stop.wait_with(&pipe, &mut pollable, |_| Err(Errno::INVAL));
    assert!(!pollable);
    stop.wait_with(&pipe, &mut pollable, |_| panic!("poll asked again"));
    assert!(!pollable);
    Ok(())
}

#[test]
fn a_burst_keeps_its_order_and_the_newest_window() -> TestResult {
    const WINDOW: u64 = 64 * 1024;
    let source = (0..1024 * 1024_u32)
        .map(|index| u8::try_from(index * 7 % 251).unwrap_or(0))
        .collect::<Vec<_>>();
    let (pipe, mut writer) = std::io::pipe()?;
    let reader = Reader::spawn(pipe, WINDOW)?;
    for piece in source.chunks(4096) {
        writer.write_all(piece)?;
    }
    drop(writer);
    let (retained, output) = reader.join()?;
    let total = u64::try_from(source.len())?;
    assert_eq!(output.total_stdout_bytes, total);
    assert_eq!(output.next_cursor, total);
    assert_eq!(output.retained_bytes, u64::try_from(retained.len())?);
    assert!(output.retained_bytes <= WINDOW);
    assert_eq!(retained, source[source.len() - retained.len()..]);
    let mut cursor = total - output.retained_bytes;
    let mut sequence = None;
    for record in &output.records {
        assert_eq!(record.cursor, cursor);
        assert!(sequence < Some(record.sequence));
        cursor += u64::try_from(record.bytes.len())?;
        sequence = Some(record.sequence);
    }
    Ok(())
}

/// An idle job's reader sleeps in `poll` instead of waking every 2 ms.
#[cfg(target_os = "linux")]
#[test]
fn an_idle_reader_does_not_wake() -> TestResult {
    let (pipe, writer) = std::io::pipe()?;
    let reader = Reader::spawn(pipe, 4096)?;
    std::thread::sleep(Duration::from_millis(100));
    let before = reader_switches()?;
    std::thread::sleep(Duration::from_millis(500));
    let woken = reader_switches()? - before;
    reader.stop.stop();
    reader.join()?;
    drop(writer);
    // Polling every 2 ms switched about 250 times in this window.
    assert!(woken <= 5, "an idle reader woke {woken} times in 500 ms");
    Ok(())
}

/// Voluntary context switches of this process's output reader threads.
#[cfg(target_os = "linux")]
fn reader_switches() -> Result<u64, Box<dyn std::error::Error>> {
    let mut switches = 0;
    for task in fs::read_dir("/proc/self/task")? {
        let task = task?.path();
        // The kernel keeps 15 bytes of a thread name. A thread that exits
        // between the listing and the read is not a reader.
        let (Ok(comm), Ok(status)) = (
            fs::read_to_string(task.join("comm")),
            fs::read_to_string(task.join("status")),
        ) else {
            continue;
        };
        if comm.trim_end() != "plak-joined-out" {
            continue;
        }
        switches += status
            .lines()
            .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
            .ok_or("no voluntary_ctxt_switches")?
            .trim()
            .parse::<u64>()?;
    }
    Ok(switches)
}
