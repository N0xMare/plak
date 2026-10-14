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
            .env("PLAK_TEST_MODEL", "offline-loopback-v1")
            .env("PLAK_TEST_SCRIPT", self.script())
            .env("PLAK_TEST_EVIDENCE", self.root.join("evidence.jsonl"));
        forward_coverage_environment(&mut command);
        std::fs::create_dir_all(self.root.join("tmp")).expect("tmp");
        command
    }
}

/// Carries the profiler's output path into the spawned binary.
///
/// The fixture clears the environment so a scripted lifecycle cannot inherit an
/// operator's settings, and that also strips `LLVM_PROFILE_FILE`. Without it a
/// coverage run reports zero for every line these tests execute, because the
/// lines run in a child process that never writes a profile. The variables are
/// forwarded only when the parent has them, so an ordinary run stays hermetic.
pub(super) fn forward_coverage_environment(command: &mut Command) {
    for name in ["LLVM_PROFILE_FILE", "CARGO_LLVM_COV_TARGET_DIR"] {
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
        if std::thread::panicking() || self.preserve.load(Ordering::Relaxed) {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.root);
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
