//! Shared real-binary offline fixtures and bounded JSONL process control.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private real-process test harness"
)]
use serde_json::{Value, json};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
pub(super) const WAIT: Duration = Duration::from_secs(45);
/// The facade's session lease (`SESSION_LEASE_SECONDS` in plakous sessions.rs).
const SESSION_LEASE: Duration = Duration::from_secs(30);
/// One disposable product home, workspace and script document.
pub(super) struct Fixture {
    pub(super) root: PathBuf,
    workspace_override: Option<PathBuf>,
    preserve: AtomicBool,
    session_lease_seconds: Option<u64>,
}

static FIXTURE_ORDINAL: AtomicU32 = AtomicU32::new(0);

impl Fixture {
    /// A fresh fixture whose offline model replays `script`.
    pub(super) fn new(script: &Value) -> Self {
        let ordinal = FIXTURE_ORDINAL.fetch_add(1, Ordering::Relaxed);
        // Under `TMPDIR`, like every other test's scratch, so the gate's
        // storage placement (`PLAK_TEST_TMPDIR`) reaches these sessions too.
        let root = std::env::temp_dir()
            .join("offline-e2e")
            .join(format!("{}-{ordinal}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fixture = Self {
            root,
            workspace_override: None,
            preserve: AtomicBool::new(false),
            session_lease_seconds: None,
        };
        std::fs::create_dir_all(fixture.home().join("agents")).expect("home");
        std::fs::create_dir_all(fixture.workspace()).expect("workspace");
        write_private(
            &fixture.home().join("config.toml"),
            "[root]\nagent = \"lead\"\n\n[orchestration]\nsubagents = false\n\n[compaction]\nenabled = true\n",
        );
        write_private(
            &fixture.home().join("agents").join("lead.toml"),
            "account = \"codex\"\nmodel = \"offline-loopback-v1\"\neffort = \"medium\"\ncomposition = \"direct\"\ndescription = \"offline loopback lead\"\n",
        );
        fixture.write_script(script);
        // The product refuses a home any other user could read.
        std::fs::set_permissions(fixture.home(), std::fs::Permissions::from_mode(0o700))
            .expect("home mode");
        std::fs::set_permissions(
            fixture.home().join("agents"),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("agents mode");
        fixture
    }

    pub(super) fn fork(&self, script: &Value) -> Self {
        let mut fixture = Self::new(script);
        fixture.workspace_override = Some(self.workspace());
        std::fs::remove_dir_all(fixture.home()).expect("empty fork home");
        copy_directory(&self.home(), &fixture.home());
        ChildRunner::at(self).fork_into(&fixture);
        fixture
    }

    /// Replaces the script document; the next process reads the new one.
    pub(super) fn write_script(&self, script: &Value) {
        std::fs::write(
            self.script(),
            serde_json::to_vec_pretty(script).expect("script json"),
        )
        .expect("script file");
    }

    pub(super) fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    pub(super) fn workspace(&self) -> PathBuf {
        self.workspace_override
            .clone()
            .unwrap_or_else(|| self.root.join("workspace"))
    }

    pub(super) fn script(&self) -> PathBuf {
        self.root.join("script.json")
    }

    /// Shortens the product session lease for tests that wait out expiry.
    /// The production constant stays 30 s; this only sets the debug-build hook.
    pub(super) fn with_session_lease_seconds(mut self, seconds: u64) -> Self {
        self.session_lease_seconds = Some(seconds);
        self
    }

    pub(super) fn preserve(&self) {
        self.preserve.store(true, Ordering::Relaxed);
    }

    /// A `plak` command bound to this fixture's home, workspace and script.
    pub(super) fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_plak"));
        command
            .current_dir(self.workspace())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("fake-home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("PLAK_HOME", self.home())
            .env("PLAK_TEST_RUNNER_DETACHED", "1")
            .env("PLAK_TEST_MODEL", "offline-loopback-v1")
            .env("PLAK_TEST_SCRIPT", self.script())
            .env("PLAK_TEST_EVIDENCE", self.root.join("evidence.jsonl"));
        if let Some(seconds) = self.session_lease_seconds {
            command.env("PLAK_TEST_SESSION_LEASE_SECONDS", seconds.to_string());
        }
        forward_coverage_environment(&mut command);
        std::fs::create_dir_all(self.root.join("tmp")).expect("tmp");
        command
    }
}

/// Carries the profiler's output path, and the gate's mid-turn prune knobs,
/// into the spawned binary.
///
/// The fixture clears the environment so a scripted lifecycle cannot inherit an
/// operator's settings, and that also strips `LLVM_PROFILE_FILE`. Without it a
/// coverage run reports zero for every line these tests execute, because the
/// lines run in a child process that never writes a profile. A sweep run with
/// `PLAK_TEST_WORKING_PRUNE_MINIMUM` prunes between rounds in every shape, and
/// `PLAK_TEST_WORKING_PRUNE_MARGIN` lowers the recency floor for that prune.
/// The variables are forwarded only when the parent has them, so an ordinary run
/// stays hermetic.
pub(super) fn forward_coverage_environment(command: &mut Command) {
    for name in [
        "LLVM_PROFILE_FILE",
        "CARGO_LLVM_COV_TARGET_DIR",
        "PLAK_TEST_WORKING_PRUNE_MINIMUM",
        "PLAK_TEST_WORKING_PRUNE_MARGIN",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

/// Writes one fixture file only the owner can read.
pub(super) fn write_private(path: &Path, body: &str) {
    std::fs::write(path, body).expect("fixture file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("mode");
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.home().join("host/runner").exists() {
            for entry in std::fs::read_dir(&self.root)
                .into_iter()
                .flatten()
                .flatten()
            {
                if entry.path().extension().is_some_and(|ext| ext == "marker") {
                    kill_marked_jobs(&entry.path());
                }
            }
        }
        kill_home_local_runner(&self.home());
        // A child runner's daemons and the jobs they started outlive every
        // host process; they end with the fixture, preserved or not.
        let runner = ChildRunner::at(self);
        if runner.home.exists() {
            for entry in std::fs::read_dir(&self.root)
                .into_iter()
                .flatten()
                .flatten()
            {
                if entry.path().extension().is_some_and(|ext| ext == "marker") {
                    kill_marked_jobs(&entry.path());
                }
            }
            runner.kill();
            if !std::thread::panicking() && !self.preserve.load(Ordering::Relaxed) {
                let _ = std::fs::remove_dir_all(&runner.home);
            }
        }
        if std::thread::panicking() || self.preserve.load(Ordering::Relaxed) {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub(super) fn kill_local_daemon_only(home: &Path) {
    for version in std::fs::read_dir(home.join("host/runner"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for lock in std::fs::read_dir(version.path().join("run"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if lock.path().extension().is_some_and(|ext| ext == "lock") {
                let daemons = daemon_openers(&lock.path());
                for pid in daemons
                    .iter()
                    .copied()
                    .filter_map(rustix::process::Pid::from_raw)
                {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                }
                wait_for_daemons(&lock.path(), &daemons);
            }
        }
    }
}

/// Reaps this fixture's local daemon and the process groups below it.
pub(super) fn kill_home_local_runner(home: &Path) {
    let versions = home.join("host/runner");
    for version in std::fs::read_dir(versions).into_iter().flatten().flatten() {
        for lock in std::fs::read_dir(version.path().join("run"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if lock.path().extension().is_none_or(|ext| ext != "lock") {
                continue;
            }
            let deadline = Instant::now() + WAIT;
            loop {
                let daemons = daemon_openers(&lock.path());
                if daemons.is_empty() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "a home-local daemon outlived fixture cleanup"
                );
                for daemon in daemons {
                    let output = Command::new("ps")
                        .args(["-axo", "pid=,ppid="])
                        .output()
                        .expect("process tree");
                    let pairs = String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .filter_map(|line| {
                            let mut columns = line.split_whitespace();
                            Some((
                                columns.next()?.parse::<i32>().ok()?,
                                columns.next()?.parse::<i32>().ok()?,
                            ))
                        })
                        .collect::<Vec<_>>();
                    let mut owned = vec![daemon];
                    let mut index = 0;
                    while index < owned.len() {
                        let parent = owned[index];
                        owned.extend(
                            pairs
                                .iter()
                                .filter(|(_, ppid)| *ppid == parent)
                                .map(|(pid, _)| *pid),
                        );
                        index += 1;
                    }
                    for pid in owned
                        .into_iter()
                        .rev()
                        .filter_map(rustix::process::Pid::from_raw)
                    {
                        let _ =
                            rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn preserved_fixture_keeps_its_evidence() {
    let fixture = Fixture::new(&json!({"version": 1}));
    let root = fixture.root.clone();
    fixture.preserve();
    drop(fixture);
    assert!(root.join("script.json").is_file());
    std::fs::remove_dir_all(root).expect("remove this test's retained fixture");
}

/// A live `plak agent` process with its stdout collected off-thread.
pub(super) struct AgentProcess {
    pub(super) child: Child,
    pub(super) stdin: Option<std::process::ChildStdin>,
    pub(super) lines: Arc<Mutex<Vec<String>>>,
    pub(super) reader: Option<std::thread::JoinHandle<()>>,
    pub(super) stderr: Arc<Mutex<String>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
}

impl AgentProcess {
    pub(super) fn start(fixture: &Fixture, arguments: &[&str]) -> Self {
        Self::start_with_drain(fixture, arguments, true)
    }

    /// Starts `plak agent`. When `drain` is false nothing reads stdout, which
    /// is the slow-reader case: the pipe fills and the writer must block
    /// rather than declare the stream failed.
    pub(super) fn start_with_drain(fixture: &Fixture, arguments: &[&str], drain: bool) -> Self {
        let mut command = fixture.command();
        command.arg("agent").args(arguments);
        Self::start_command(command, drain)
    }

    pub(super) fn start_command(mut command: Command, drain: bool) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("plak agent spawned");
        let stdin = child.stdin.take();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let mut stderr_pipe = child.stderr.take().expect("stderr pipe");
        let stderr_sink = Arc::clone(&stderr_text);
        let stderr_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr_pipe.read_to_string(&mut text);
            *stderr_sink.lock().unwrap() = text;
        });
        let reader = drain.then(|| {
            let stdout = child.stdout.take().expect("stdout pipe");
            let sink = Arc::clone(&lines);
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    sink.lock().unwrap().push(line);
                }
            })
        });
        Self {
            child,
            stdin,
            lines,
            reader,
            stderr: stderr_text,
            stderr_reader: Some(stderr_reader),
        }
    }

    pub(super) fn send(&mut self, frame: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{frame}").expect("frame written");
        stdin.flush().expect("frame flushed");
    }

    pub(super) fn close_stdin(&mut self) {
        self.stdin = None;
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn snapshot(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    /// Waits until a decoded record satisfies `predicate`, or fails.
    pub(super) fn wait_for(&self, what: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            for line in self.snapshot() {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && predicate(&value)
                {
                    return value;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "{what} never arrived; stdout was:\n{}\nstderr was:\n{}",
            self.snapshot().join("\n"),
            self.stderr.lock().unwrap()
        );
    }

    pub(super) fn wait_for_completed_turns(&self, count: usize) {
        self.wait_for("completed turns", |record| {
            record["kind"] == "assistant_done"
                && self
                    .snapshot()
                    .iter()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .filter(|record| record["kind"] == "assistant_done")
                    .count()
                    >= count
        });
    }

    /// Waits for exit and returns the code plus the complete stream.
    pub(super) fn finish(mut self) -> AgentOutcome {
        self.stdin = None;
        let deadline = Instant::now() + WAIT;
        let status = loop {
            match self.child.try_wait().expect("wait") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    panic!(
                        "plak agent never exited; stdout was:\n{}",
                        self.snapshot().join("\n")
                    );
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        };
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
        AgentOutcome {
            status,
            lines: self.snapshot(),
            stderr: self.stderr.lock().unwrap().clone(),
        }
    }
}

pub(super) struct AgentOutcome {
    pub(super) status: std::process::ExitStatus,
    pub(super) lines: Vec<String>,
    pub(super) stderr: String,
}

impl AgentOutcome {
    pub(super) fn records(&self) -> Vec<Value> {
        self.lines
            .iter()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|_| panic!("agent wrote a non-JSON line: {line}"))
            })
            .collect()
    }

    pub(super) fn kinds(&self) -> Vec<String> {
        self.records()
            .iter()
            .filter_map(|record| record.get("kind").and_then(Value::as_str))
            .map(str::to_owned)
            .collect()
    }

    /// The one terminal record the protocol allows.
    pub(super) fn assert_exactly_one_terminal_record(&self) {
        let kinds = self.kinds();
        let byes = kinds.iter().filter(|kind| *kind == "bye").count();
        assert_eq!(
            byes,
            1,
            "expected exactly one `bye`, saw {byes}; stream was:\n{}\nstderr:\n{}",
            self.lines.join("\n"),
            self.stderr
        );
        assert_eq!(
            kinds.last().map(String::as_str),
            Some("bye"),
            "`bye` must be the last record; stream was:\n{}",
            self.lines.join("\n")
        );
    }

    pub(super) fn assistant_text(&self) -> Vec<String> {
        self.records()
            .iter()
            .filter(|record| record.get("kind").and_then(Value::as_str) == Some("assistant"))
            .filter_map(|record| record.get("text").and_then(Value::as_str))
            .map(str::to_owned)
            .collect()
    }

    pub(super) fn session_id(&self) -> String {
        self.records()
            .iter()
            .find(|record| record.get("kind").and_then(Value::as_str) == Some("hello"))
            .and_then(|record| record.get("session_id").and_then(Value::as_str))
            .expect("hello carried a session id")
            .to_owned()
    }
}

pub(super) fn prompt_frame(text: &str) -> Value {
    json!({"v": 1, "kind": "prompt", "text": text})
}

pub(super) fn quit_frame() -> Value {
    json!({"v": 1, "kind": "quit"})
}

/// Blocks until the offline model has entered its provider operation.
///
/// The fsynced evidence log gets its first row when the model announces the
/// dispatch, which is strictly after the face installed its signal watcher.
/// Sleeping a fixed interval instead would race process start under gate load,
/// and a signal that lands before the watcher is a different test.
pub(super) fn wait_for_provider_dispatch(fixture: &Fixture) {
    let evidence = fixture.root.join("evidence.jsonl");
    let deadline = Instant::now() + WAIT;
    loop {
        if std::fs::read(&evidence).is_ok_and(|bytes| !bytes.is_empty()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the offline model never entered its provider operation"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn reopen_until_the_lease_clears(fixture: &Fixture, session: &str) -> AgentProcess {
    reopen_result(fixture, session)
        .unwrap_or_else(|outcome| panic!("session refused: {}", outcome.lines.join("\n")))
}

pub(super) fn reopen_result(
    fixture: &Fixture,
    session: &str,
) -> Result<AgentProcess, AgentOutcome> {
    reopen_configured(fixture, session, |_, _| {})
}

pub(super) fn reopen_configured(
    fixture: &Fixture,
    session: &str,
    configure: impl Fn(&mut Command, usize),
) -> Result<AgentProcess, AgentOutcome> {
    // A reopen may first wait out the previous owner's 30 s session lease, so
    // the ready wait gets the lease on top of the usual load allowance.
    let deadline = Instant::now() + SESSION_LEASE + WAIT;
    let mut refusals = 0;
    loop {
        let mut command = fixture.command();
        command.args(["agent", "--session", session]);
        configure(&mut command, refusals);
        let agent = AgentProcess::start_command(command, true);
        // A loaded host can take longer than a fixed few seconds to recover
        // and report ready; only an exit (bye) or the overall deadline ends
        // the wait, so a slow open is never mistaken for a refusal.
        let ready = loop {
            let seen = agent.snapshot();
            if seen.iter().any(|line| line.contains("\"kind\":\"ready\"")) {
                break true;
            }
            if seen.iter().any(|line| line.contains("\"kind\":\"bye\"")) {
                break false;
            }
            assert!(
                Instant::now() < deadline,
                "the reopened session neither reported ready nor exited"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        if ready {
            return Ok(agent);
        }
        refusals += 1;
        let outcome = agent.finish();
        let why = outcome
            .records()
            .iter()
            .filter(|record| record["kind"] == "error")
            .filter_map(|record| record["message"].as_str())
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .join("; ");
        if !why.contains("retry in about")
            && !why
                .to_ascii_lowercase()
                .split(|c: char| !c.is_ascii_alphabetic())
                .any(|word| matches!(word, "lease" | "busy"))
        {
            return Err(outcome);
        }
        assert!(
            Instant::now() < deadline,
            "the session never reopened after {refusals} refusals; \
             the last one said: {why:?} (exit {:?}), stderr:\n{}",
            outcome.status.code(),
            outcome.stderr
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

pub(super) fn copy_directory(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("copy directory");
    std::fs::set_permissions(target, std::fs::metadata(source).unwrap().permissions()).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).expect("copy fixture file");
        }
    }
}

/// Only the recorded daemon, not a host briefly probing the startup lock.
/// Confirm it still has this fixture's file open before authorizing a signal;
/// a stale pid in an unlocked file is not authority to kill a process.
fn daemon_openers(path: &Path) -> Vec<i32> {
    let Some(holder) = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
    else {
        return Vec::new();
    };
    opened_by(path)
        .into_iter()
        .filter(|pid| *pid == holder)
        .collect()
}

fn wait_for_daemons(path: &Path, daemons: &[i32]) {
    let deadline = Instant::now() + WAIT;
    // The live host may already have redialed and started a successor. Only
    // the processes we killed must disappear, not every future opener.
    while opened_by(path).iter().any(|pid| daemons.contains(pid)) {
        assert!(Instant::now() < deadline, "the old daemon did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn daemon_exit_wait_ignores_a_successor_and_a_startup_probe() {
    let fixture = Fixture::new(&json!({"version": 1}));
    let runner = ChildRunner::of(&fixture);
    let old = runner.start();
    let lock = std::fs::read_dir(runner.home.join("runner/run"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "lock"))
        .unwrap()
        .canonicalize()
        .unwrap();
    let probe = std::fs::File::open(&lock).unwrap();
    assert!(opened_by(&lock).contains(&i32::try_from(std::process::id()).unwrap()));
    assert_eq!(daemon_openers(&lock), [old]);
    runner.kill();
    std::fs::remove_file(lock.with_extension("sock")).unwrap();
    // Force the race deterministically: the successor is already listening
    // before the old daemon's exit is observed by the waiter.
    let successor = runner.start();
    assert_ne!(old, successor);
    wait_for_daemons(&lock, &[old]);
    assert_eq!(daemon_openers(&lock), [successor]);
    runner.kill();
    assert_eq!(daemon_openers(&lock), []);
    drop(probe);
}

/// The processes that have `path` open, from their descriptor tables.
#[cfg(target_os = "linux")]
fn opened_by(path: &Path) -> Vec<i32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            std::fs::read_dir(entry.path().join("fd"))
                .ok()?
                .flatten()
                .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == path))
                .then_some(pid)
        })
        .collect()
}

/// The processes that have `path` open, as `lsof` lists them.
#[cfg(target_os = "macos")]
fn opened_by(path: &Path) -> Vec<i32> {
    Command::new("lsof")
        .args(["-t", "--"])
        .arg(path)
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Kills each job a marker file names that is still that job: a process
/// whose command line names the marker, so a pid reused on a busy machine
/// is never signalled.
fn kill_marked_jobs(marker: &Path) {
    let marker_name = marker.display().to_string();
    for line in std::fs::read_to_string(marker).unwrap_or_default().lines() {
        let Some(pid) = line
            .split_whitespace()
            .next()
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        let command = Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        if pid > 1 && command.contains(&marker_name) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            let deadline = Instant::now() + WAIT;
            loop {
                let still_this_job = Command::new("ps")
                    .args(["-o", "command=", "-p", &pid.to_string()])
                    .output()
                    .is_ok_and(|output| {
                        String::from_utf8_lossy(&output.stdout).contains(&marker_name)
                    });
                if !still_this_job {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "a marked job outlived fixture cleanup"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// A child runner for one fixture (spec 01 §8.5): a runner daemon's own
/// product home beside the fixture, which pairs the fixture's host, serves
/// the fixture's workspace, and records the daemon this harness started.
/// The host reaches it through `plak runner connect --stdio`.
pub(super) struct ChildRunner {
    /// The daemon's product home. Socket paths are short, so it lives
    /// directly under the temporary directory rather than in the fixture.
    pub(super) home: PathBuf,
    /// Where the daemon's pid is recorded, beside the fixture.
    pid_file: PathBuf,
}

/// The host identity every child-runner fixture binds as.
pub(super) const CHILD_RUNNER_HOST: &str = "00000000-0000-4000-8000-0000000000c7";

/// The fixture file naming the child runner's daemon. Not a `.pid` file: a
/// cut's lifecycle kills those when the host dies, and the daemon outlives
/// the host.
const CHILD_RUNNER_DAEMON: &str = "child-runner.daemon";

impl ChildRunner {
    /// The child runner of `fixture`: where its home and daemon record are.
    pub(super) fn at(fixture: &Fixture) -> Self {
        let name = fixture
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_owned();
        Self {
            home: std::env::temp_dir().join(format!("rh-{name}")),
            pid_file: fixture.root.join(CHILD_RUNNER_DAEMON),
        }
    }

    /// The child runner of `fixture`, configured to serve its workspace and
    /// selected by its script. Nothing runs until [`Self::start`] or the
    /// host's bridge starts the daemon.
    pub(super) fn of(fixture: &Fixture) -> Self {
        let runner = Self::at(fixture);
        let directory = runner.home.join("runner");
        std::fs::create_dir_all(&directory).unwrap();
        for path in [&runner.home, &directory] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        write_private(
            &directory.join("runner.toml"),
            &format!(
                "roots = [{:?}]\nhosts = [{CHILD_RUNNER_HOST:?}]\npath = {:?}\nidle_exit_seconds = 20\n",
                fixture
                    .workspace()
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
        );
        let mut script: Value = serde_json::from_slice(&std::fs::read(fixture.script()).unwrap())
            .expect("fixture script");
        script["child_runner"] = json!({"home": runner.home, "host_id": CHILD_RUNNER_HOST});
        fixture.write_script(&script);
        runner
    }

    /// Gives `fork` this runner's identity and owner floors, as the same
    /// runner machine: a session bound to a runner reopens only on it. The
    /// fork's daemon is its own.
    fn fork_into(&self, fork: &Fixture) {
        let from = self.home.join("runner");
        if !from.is_dir() {
            return;
        }
        let to = Self::at(fork).home.join("runner");
        std::fs::create_dir_all(&to).unwrap();
        for path in [to.parent().unwrap(), to.as_path()] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for name in ["runner-id", "owners.json"] {
            if from.join(name).is_file() {
                std::fs::copy(from.join(name), to.join(name)).unwrap();
            }
        }
    }

    /// Starts the daemon, in its own process group as `connect` would, and
    /// waits until it listens.
    pub(super) fn start(&self) -> i32 {
        let log = std::fs::File::create(self.home.join("serve-harness.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_plak"));
        command
            .args(["runner", "serve"])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("PLAK_HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        forward_coverage_environment(&mut command);
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().expect("runner daemon");
        let pid = i32::try_from(child.id()).unwrap();
        std::fs::write(&self.pid_file, pid.to_string()).unwrap();
        let run = self.home.join("runner").join("run");
        let deadline = Instant::now() + WAIT;
        let log =
            || std::fs::read_to_string(self.home.join("serve-harness.log")).unwrap_or_default();
        while !std::fs::read_dir(&run).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "sock"))
        }) {
            // A daemon that cannot serve, such as one whose socket path is
            // too long for the platform, says why and exits.
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "the runner daemon exited ({status}) before it listened: {}",
                    log()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the runner daemon never listened: {}",
                log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // From here the harness only signals the daemon and reaps it once.
        std::mem::forget(child);
        pid
    }

    /// The daemon this harness started last, while it lives. It is this
    /// harness's child, so an exit is reaped here rather than mistaken for
    /// a live zombie.
    pub(super) fn pid(&self) -> Option<i32> {
        let pid = std::fs::read_to_string(&self.pid_file)
            .ok()?
            .trim()
            .parse::<i32>()
            .ok()?;
        let child = rustix::process::Pid::from_raw(pid)?;
        // Exited, or no longer this harness's child to wait for: every
        // recorded daemon is its child, so either way it is gone and its pid
        // may already be another's.
        if let Ok(None) =
            rustix::process::waitpid(Some(child), rustix::process::WaitOptions::NOHANG)
        {
            return Some(pid);
        }
        let _ = std::fs::remove_file(&self.pid_file);
        None
    }

    /// Every recorded daemon still holding open a socket's lock file in
    /// this runner home, including one started by the host's bridge.
    /// Asking takes no lock and excludes transient startup probes.
    fn holders(&self) -> Vec<i32> {
        let run = self.home.join("runner").join("run");
        std::fs::read_dir(run)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lock"))
            .filter_map(|entry| entry.path().canonicalize().ok())
            .flat_map(|lock| daemon_openers(&lock))
            .collect()
    }

    /// Kills every daemon of this runner home, each with its whole process
    /// group (the daemon, not its jobs, which run in groups of their own),
    /// and waits until none has its lock file open.
    pub(super) fn kill(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            let mut daemons = self.holders();
            daemons.extend(self.pid());
            if daemons.is_empty() {
                break;
            }
            for pid in daemons {
                let Some(pid) = rustix::process::Pid::from_raw(pid) else {
                    continue;
                };
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                // Reaps a daemon this harness started; any other is not its child.
                let _ = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());
            }
            let _ = std::fs::remove_file(&self.pid_file);
            if Instant::now() >= deadline {
                // A fixture dropped by a failing test reports that failure.
                assert!(
                    std::thread::panicking(),
                    "a runner daemon of {} outlived SIGKILL",
                    self.home.display()
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kills the daemon and removes its home.
    pub(super) fn remove(&self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}
