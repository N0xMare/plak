//! Offline end-to-end lifecycles of the real `plak` binary.
//!
//! Every test here spawns the shipped executable and drives one of its three
//! faces — the CLI (`plak run`), the agent JSONL stream (`plak agent`) and the
//! terminal application — through a complete lifecycle against the scripted
//! offline model (`crate::test_hooks`, feature `test-hooks`). No provider, no
//! credential and no network is involved, and the script is a JSON document
//! this file writes.
//!
//! These exist because a whole class of defects lives only in the spawned
//! process: how a signal ends it, what its writer does when the reader is slow,
//! whether a failed shutdown still terminates the stream, and whether a session
//! an earlier process wrote reopens. An in-process test cannot reach any of it.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private process-level test harness"
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::offline_support::*;

/// The stall one mid-turn interruption needs. Long enough that the signal
/// lands inside the provider operation on a loaded machine, short enough that
/// a test that misses it still finishes.
const MID_TURN_STALL_MS: u64 = 3_000;

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// Two tool round trips and then an exact answer.
fn two_tool_round_trips(answer: &str) -> Value {
    json!({
        "version": 1,
        "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": answer}, "usage": {"output": 9}}
        ]
    })
}

/// One answer, preceded by a stall long enough to interrupt.
fn stalled_answer() -> Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        // Deaf to cancellation on purpose. Every caller of this helper cuts the
        // process mid-request and then asserts on what the cut left behind, and
        // a stall that observes cancellation resolves the request before the
        // process dies, so the reopen finds nothing outstanding and has nothing
        // to abandon. Making the stall cancellable is right in general, and it
        // silently emptied this helper's premise until the deadline case
        // failed.
        "stall_ignores_cancellation": true,
        "steps": [
            {"output": {"text": "the stalled answer"}, "stall_ms": MID_TURN_STALL_MS}
        ]
    })
}

// ---------------------------------------------------------------------------
// CLI face
// ---------------------------------------------------------------------------

/// The finished output of one CLI invocation.
struct CliRun {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl CliRun {
    fn session_id(&self) -> String {
        self.stdout
            .lines()
            .find_map(|line| line.strip_prefix("session_id="))
            .expect("printed session id")
            .to_owned()
    }
}

fn run_cli(fixture: &Fixture, arguments: &[&str]) -> CliRun {
    let mut child = spawn_cli(fixture, arguments);
    wait_with_output(&mut child)
}

/// Runs a short CLI command with stdout attached to a real pseudoterminal.
/// This is separate from `TerminalChild`: these commands exit on their own
/// and must exercise stdout's actual `IsTerminal` result, not a test seam.
fn run_cli_pty(fixture: &Fixture, arguments: &[&str]) -> (portable_pty::ExitStatus, Vec<u8>) {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("CLI pty opened");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_plak"));
    command.args(arguments);
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("HOME", fixture.root.join("fake-home"));
    command.env("TMPDIR", fixture.root.join("tmp"));
    command.env("PLAK_HOME", fixture.home());
    command.env("PLAK_TEST_MODEL", "offline-loopback-v1");
    command.env("PLAK_TEST_SCRIPT", fixture.script());
    command.env("PLAK_TEST_EVIDENCE", fixture.root.join("evidence.jsonl"));
    command.env("TERM", "xterm-256color");
    command.env("NO_COLOR", "1");
    command.env("LANG", "C.UTF-8");
    command.cwd(fixture.workspace());
    let mut reader = pair.master.try_clone_reader().expect("CLI pty reader");
    let capture = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("CLI pty output");
        bytes
    });
    let mut child = pair.slave.spawn_command(command).expect("CLI pty spawned");
    drop(pair.slave);
    let status = child.wait().expect("CLI pty waited");
    drop(pair.master);
    let bytes = capture.join().expect("CLI pty reader joined");
    (status, bytes)
}

/// Starts one fresh CLI turn and returns the live child.
///
/// Stdin carries one `y`: a first native run in a fresh home asks for the
/// revision-pinned compatibility acknowledgement, and the answer persists in
/// the home for every later run. Nothing else on this face reads stdin.
fn spawn_cli(fixture: &Fixture, arguments: &[&str]) -> Child {
    let mut child = fixture
        .command()
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("plak spawned");
    let mut stdin = child.stdin.take().expect("stdin pipe");
    let _ = stdin.write_all(b"y\n");
    let _ = stdin.flush();
    child
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

fn signal(pid: u32, signal: i32) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("kill spawned");
    assert!(status.success(), "kill -{signal} {pid} failed");
}

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
const SIGHUP: i32 = 1;
const SIGKILL: i32 = 9;
/// `ProcessOutcome::Interrupted`, which is what a signalled stop exits with.
const SIGNALLED_EXIT_CODE: i32 = 4;

// ---------------------------------------------------------------------------
// CLI lifecycles
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_cli_session_runs_two_tool_round_trips_and_answers() {
    let fixture = Fixture::new(&two_tool_round_trips("the offline answer is 42"));
    let run = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "do the thing",
        ],
    );
    assert!(
        run.status.success(),
        "run failed: {}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("the offline answer is 42"),
        "stdout did not carry the scripted answer: {}",
        run.stdout
    );
    let tool_lines = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("plak: tool") && line.contains("loopback_effect"))
        .count();
    assert_eq!(tool_lines, 2, "stderr was:\n{}", run.stderr);
}

/// `plak run --account openai --model gpt-6-luna --effort none` completes on
/// the scripted model, stores the spelling `none`, and `plak run --session`
/// continues that row without rewriting it. The API request body is asserted
/// by the hosted loopback test: this hook replaces the host before the wire.
#[test]
fn openai_effort_none_completes_and_resume_keeps_the_stored_spelling() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "OK"}}]
    }));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "openai",
            "--model",
            "gpt-6-luna",
            "--effort",
            "none",
            "--print-session-id",
            "--",
            "Reply OK",
        ],
    );
    assert!(
        first.status.success(),
        "run failed: {}\n{}",
        first.stdout,
        first.stderr
    );
    assert!(first.stdout.contains("OK\n"), "{}", first.stdout);
    let session = first.session_id();
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let effort: String = rusqlite::Connection::open(paths.product_database())
        .expect("product database")
        .query_row(
            "SELECT effort FROM sessions WHERE id = ?1",
            [&session],
            |row| row.get(0),
        )
        .expect("stored effort");
    assert_eq!(effort, "none");

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "again"}}]
    }));
    let resumed = run_cli(
        &fixture,
        &["run", "--session", &session, "--", "Reply again"],
    );
    assert!(
        resumed.status.success(),
        "resume failed: {}\n{}",
        resumed.stdout,
        resumed.stderr
    );
    assert!(resumed.stdout.contains("again\n"), "{}", resumed.stdout);
    let kept: String = rusqlite::Connection::open(paths.product_database())
        .expect("product database")
        .query_row(
            "SELECT effort FROM sessions WHERE id = ?1",
            [&session],
            |row| row.get(0),
        )
        .expect("stored effort");
    assert_eq!(kept, "none");
}

#[test]
fn a_cli_resume_delivers_a_second_prompt_into_the_persisted_session() {
    let fixture = Fixture::new(&two_tool_round_trips("first answer"));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "first prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let session = first.session_id();

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "second answer"}}]
    }));
    let second = run_cli(
        &fixture,
        &["run", "--session", &session, "--", "second prompt"],
    );
    assert!(
        second.status.success(),
        "resume failed: {}\n{}",
        second.stdout,
        second.stderr
    );
    assert!(
        second.stdout.contains("second answer"),
        "resumed stdout was: {}",
        second.stdout
    );
}

/// The short id `plak resume` accepts continues a session on `plak run`, and
/// the answer ends with a line break so the shell prompt starts fresh.
#[test]
fn a_cli_run_continues_a_session_by_its_short_id() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "first answer"}}]
    }));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "first prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    assert!(
        first.stdout.ends_with("first answer\n"),
        "{:?}",
        first.stdout
    );
    let session = first.session_id();

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "second answer"}}]
    }));
    let second = run_cli(
        &fixture,
        &["run", "--session", &session[..8], "--", "second prompt"],
    );
    assert!(
        second.status.success(),
        "short-id resume failed: {}\n{}",
        second.stdout,
        second.stderr
    );
    assert_eq!(second.stdout, "second answer\n");
}

#[test]
fn a_cli_turn_stopped_by_sigterm_announces_and_exits_with_the_signal_code() {
    let fixture = Fixture::new(&stalled_answer());
    let mut child = spawn_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--",
            "stall please",
        ],
    );
    wait_for_provider_dispatch(&fixture);
    signal(child.id(), SIGTERM);
    let output = wait_with_output(&mut child);
    assert_eq!(
        output.status.code(),
        Some(SIGNALLED_EXIT_CODE),
        "a signalled CLI stop must not exit in silence; stderr was:\n{}",
        output.stderr
    );
    assert!(
        output.stderr.contains("[terminated]"),
        "the signal announcement is missing; stderr was:\n{}",
        output.stderr
    );
}

/// One CLI turn whose stderr is collected from the first byte it writes.
///
/// [`wait_with_output`] takes the pipes only once the process is dead, which
/// is enough to read a finished run but cannot say when the run started. This
/// reads from the moment of spawn, so a test can anchor on the process rather
/// than on the wall clock.
struct WatchedCli {
    child: Child,
    stderr: Arc<Mutex<String>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl WatchedCli {
    /// Blocks until the process has written its first byte of stderr.
    ///
    /// That byte is the earliest observable proof that plak's own code is
    /// running. Everything before it — `exec`, dynamic loading, process start
    /// — happens with the kernel's default disposition, and no code inside the
    /// process can arm a handler ahead of its own first instruction. A debug
    /// build of this binary measures between 0.6 and 2 seconds to get there
    /// from cold, so a sweep counting from spawn would be timing the loader
    /// rather than the face.
    fn wait_until_started(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            if !self.stderr.lock().expect("stderr sink").is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the CLI face never wrote anything"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Signals the process, waits for it, and returns its status and stderr.
    fn stop(&mut self, signal_number: i32) -> (Option<i32>, String) {
        signal(self.child.id(), signal_number);
        let deadline = Instant::now() + WAIT;
        let status = loop {
            match self.child.try_wait().expect("wait") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    panic!("the CLI face never exited after its signal");
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let text = self.stderr.lock().expect("stderr sink").clone();
        (status.code(), text)
    }
}

impl Drop for WatchedCli {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts one CLI turn and watches its stderr from the first byte.
fn spawn_cli_watched(fixture: &Fixture, arguments: &[&str]) -> WatchedCli {
    let mut child = fixture
        .command()
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("plak spawned");
    let mut stdin = child.stdin.take().expect("stdin pipe");
    let _ = stdin.write_all(b"y\n");
    let _ = stdin.flush();
    drop(stdin);
    // Nothing reads this face's stdout here, but an undrained pipe would stall
    // the writer, so it is discarded rather than left to fill.
    let mut discarded = child.stdout.take().expect("stdout pipe");
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = discarded.read_to_end(&mut sink);
    });
    let stderr = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&stderr);
    let mut pipe = child.stderr.take().expect("stderr pipe");
    let reader = std::thread::spawn(move || {
        let mut buffer = [0_u8; 512];
        while let Ok(read) = pipe.read(&mut buffer) {
            if read == 0 {
                return;
            }
            sink.lock()
                .expect("stderr sink")
                .push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
    });
    WatchedCli {
        child,
        stderr,
        reader: Some(reader),
    }
}

/// A stop is handled wherever in a running turn it lands, not only once the
/// model has been dispatched.
///
/// The watcher used to be installed by the face itself, and the face exists
/// only once the composition root, retirement staging, the compatibility
/// acknowledgement, admission and the store open have all run. A stop that
/// arrived inside that window met the default disposition: the process died at
/// once, with no announcement, an empty stream and a branch head nobody could
/// resume from. Live, that showed up as two rows in twelve dying to `-1` and
/// `-15` while the other ten announced and exited 4 on the same signals, and
/// the width of the window depends on machine load rather than on the case.
///
/// The sweep anchors on the first byte the process writes and walks forward
/// from there, so the early window is held to the same assertion as the
/// dispatched one. It deliberately does not sweep before that byte: process
/// start is not something the process can arm a handler ahead of.
#[test]
fn a_cli_turn_handles_a_stop_wherever_in_the_running_turn_it_lands() {
    for after_start in [0_u64, 10, 40, 160, 640] {
        let fixture = Fixture::new(&stalled_answer());
        let mut child = spawn_cli_watched(
            &fixture,
            &[
                "run",
                "--account",
                "codex",
                "--model",
                "offline-loopback-v1",
                "--effort",
                "medium",
                "--",
                "stall please",
            ],
        );
        child.wait_until_started();
        std::thread::sleep(Duration::from_millis(after_start));
        let (code, stderr) = child.stop(SIGTERM);
        assert_eq!(
            code,
            Some(SIGNALLED_EXIT_CODE),
            "a stop {after_start} ms after the face started died to the default \
             disposition; stderr was:\n{stderr}"
        );
        assert!(
            stderr.contains("[terminated]"),
            "a stop {after_start} ms after the face started was not announced; \
             stderr was:\n{stderr}"
        );
    }
}

#[test]
fn a_cli_turn_stopped_by_sighup_announces_the_hangup() {
    let fixture = Fixture::new(&stalled_answer());
    let mut child = spawn_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--",
            "stall please",
        ],
    );
    wait_for_provider_dispatch(&fixture);
    signal(child.id(), SIGHUP);
    let output = wait_with_output(&mut child);
    assert_eq!(output.status.code(), Some(SIGNALLED_EXIT_CODE));
    assert!(
        output.stderr.contains("[hangup]"),
        "the hangup announcement is missing; stderr was:\n{}",
        output.stderr
    );
}

#[test]
fn a_cli_turn_interrupted_by_sigint_leaves_a_session_a_later_process_resumes() {
    let fixture = Fixture::new(&stalled_answer());
    let mut child = spawn_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "stall please",
        ],
    );
    wait_for_provider_dispatch(&fixture);
    signal(child.id(), SIGINT);
    let output = wait_with_output(&mut child);
    let session = output
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("session_id="))
        .expect("session id was printed before the turn")
        .to_owned();
    assert_ne!(
        output.status.code(),
        None,
        "an interrupted CLI turn must exit, not die on the signal"
    );

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "resumed after interrupt"}}]
    }));
    let resumed = run_cli(&fixture, &["run", "--session", &session, "--", "carry on"]);
    assert!(
        resumed.status.success(),
        "an interrupted session must stay resumable: {}\n{}",
        resumed.stdout,
        resumed.stderr
    );
    assert!(resumed.stdout.contains("resumed after interrupt"));
}

fn wait_with_output(child: &mut Child) -> CliRun {
    let mut stdout_pipe = child.stdout.take().expect("stdout");
    let mut stderr_pipe = child.stderr.take().expect("stderr");
    let stdout_handle = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout_pipe.read_to_string(&mut text);
        text
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr_pipe.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + WAIT;
    let status = loop {
        match child.try_wait().expect("wait") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("the CLI face never exited after its signal");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    CliRun {
        status,
        stdout: stdout_handle.join().unwrap_or_default(),
        stderr: stderr_handle.join().unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// JSONL lifecycles
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_jsonl_session_runs_two_tool_round_trips_and_answers() {
    let fixture = Fixture::new(&two_tool_round_trips("the offline answer is 42"));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("do the thing"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(
        outcome.assistant_text(),
        vec!["the offline answer is 42".to_owned()]
    );
    let tool_records = outcome
        .records()
        .iter()
        .filter(|record| record["kind"] == "tool" && record["state"] == "done")
        .count();
    assert_eq!(tool_records, 2, "stream:\n{}", outcome.lines.join("\n"));
}

/// One turn that runs a real command which exits nonzero, then answers.
fn failing_command_script(answer: &str) -> Value {
    json!({
        "version": 1,
        "process_tools": true,
        "steps": [
            {"output": {"tool": {"name": "process_session_start", "arguments": {
                "command": "printf 'building\\n'; exit 3",
                "workdir": ".",
                "login": false,
                "tty": false,
                "keep_stdin_open": false,
                "yield_time_ms": 10000,
                "max_output_bytes": 8192
            }}}, "finish": "tool_calls"},
            {"output": {"text": answer}}
        ]
    })
}

/// The tool call succeeded, the command it ran did not. The `tool`
/// record names how the command ended instead of reporting `done`.
#[test]
fn a_command_that_exits_nonzero_is_a_failed_tool_record_naming_its_exit() {
    let fixture = Fixture::new(&failing_command_script("the build failed"));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("build it"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    let terminal = outcome
        .records()
        .into_iter()
        .rfind(|record| record["kind"] == "tool" && record["name"] == "process_session_start")
        .expect("the command's tool record");
    assert_eq!(
        terminal["state"],
        "failed:exit 3",
        "stream:\n{}",
        outcome.lines.join("\n")
    );
}

#[test]
fn a_jsonl_session_written_by_an_earlier_process_reopens_for_a_second_prompt() {
    let fixture = Fixture::new(&two_tool_round_trips("first answer"));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("first prompt"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let first = agent.finish();
    assert!(first.status.success(), "stderr:\n{}", first.stderr);
    let session = first.session_id();

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "second answer"}}]
    }));
    let mut resumed = AgentProcess::start(&fixture, &["--session", &session]);
    resumed.wait_for("ready", |record| record["kind"] == "ready");
    resumed.send(&prompt_frame("second prompt"));
    resumed.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    resumed.send(&quit_frame());
    let outcome = resumed.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(outcome.session_id(), session);
    assert_eq!(outcome.assistant_text(), vec!["second answer".to_owned()]);
}

#[test]
fn a_jsonl_stream_stopped_by_sigterm_mid_turn_writes_exactly_one_terminal_record() {
    let fixture = Fixture::new(&stalled_answer());
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("stall please"));
    agent.wait_for("running", |record| {
        record["kind"] == "status" && record["phase"] == "running"
    });
    signal(agent.id(), SIGTERM);
    agent.close_stdin();
    let outcome = agent.finish();
    outcome.assert_exactly_one_terminal_record();
    assert!(
        matches!(outcome.status.code(), Some(0 | SIGNALLED_EXIT_CODE)),
        "a signalled JSONL stop must exit with a code, not die silently; code was {:?}, stderr:\n{}",
        outcome.status.code(),
        outcome.stderr
    );
}

#[test]
fn a_jsonl_stream_stopped_by_sighup_mid_turn_writes_exactly_one_terminal_record() {
    let fixture = Fixture::new(&stalled_answer());
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("stall please"));
    agent.wait_for("running", |record| {
        record["kind"] == "status" && record["phase"] == "running"
    });
    signal(agent.id(), SIGHUP);
    agent.close_stdin();
    let outcome = agent.finish();
    outcome.assert_exactly_one_terminal_record();
    assert!(matches!(
        outcome.status.code(),
        Some(0 | SIGNALLED_EXIT_CODE)
    ));
}

/// Commands piped back to back are each answered under their own id.
///
/// The `quit` right behind the `compact` interrupts it, so the compact's
/// answer may be its interruption rather than its outcome; either way it is
/// the compact's, and no answer carries the id of the command read last.
#[test]
fn back_to_back_jsonl_commands_are_answered_under_their_own_ids() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "unused"}}]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&json!({"v": 1, "kind": "compact", "id": "c"}));
    agent.send(&json!({"v": 1, "kind": "recover", "id": "e"}));
    agent.send(&json!({"v": 1, "kind": "quit", "id": "q"}));
    let outcome = agent.finish();
    outcome.assert_exactly_one_terminal_record();
    let records = outcome.records();
    let stream = outcome.lines.join("\n");
    let answers = |id: &str| {
        records
            .iter()
            .filter(|record| {
                record["id"] == id
                    && (record["kind"] == "error"
                        || record["kind"] == "status" && record["phase"] == "note")
            })
            .count()
    };
    assert!(answers("c") >= 1, "compact went unanswered:\n{stream}");
    assert!(answers("e") >= 1, "recover went unanswered:\n{stream}");
    assert_eq!(
        answers("q"),
        0,
        "an answer took the last command's id:\n{stream}"
    );
    for record in &records {
        if let Some(message) = record["message"].as_str() {
            if message.contains("compact") {
                assert_eq!(record["id"], "c", "{stream}");
            }
            if message.contains("nothing to recover") {
                assert_eq!(record["id"], "e", "{stream}");
            }
        }
    }
    let bye = records
        .iter()
        .find(|record| record["kind"] == "bye")
        .expect("bye");
    assert_eq!(bye["id"], "q", "{stream}");
}

/// A manual compact always answers under its own id with a reason, never a
/// bare failed row. (Which reason depends on the route; the offline model has
/// no compaction configuration, and the wording per outcome is pinned by the
/// host's compact tests.)
#[test]
fn a_manual_compact_answers_with_its_reason() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "unused"}}]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&json!({"v": 1, "kind": "compact", "id": "c"}));
    let answer = agent.wait_for("compact answer", |record| {
        record["id"] == "c"
            && (record["kind"] == "error"
                || record["kind"] == "status" && record["phase"] == "note")
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    outcome.assert_exactly_one_terminal_record();
    let text = answer["detail"]
        .as_str()
        .or_else(|| answer["message"].as_str())
        .unwrap_or_default();
    assert!(!text.trim().is_empty(), "{answer}");
}

/// Ctrl-C (`SIGINT`) on `plak agent` interrupts the running turn at once and
/// ends the stream with its one terminal record, even with stdin still open.
#[test]
fn a_jsonl_turn_interrupted_by_sigint_stops_promptly_with_exit_4() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "never delivered"}, "stall_ms": 30_000}]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("stall please"));
    agent.wait_for("running", |record| {
        record["kind"] == "status" && record["phase"] == "running"
    });
    wait_for_provider_dispatch(&fixture);
    let signalled = Instant::now();
    signal(agent.id(), SIGINT);
    agent.wait_for("bye", |record| record["kind"] == "bye");
    assert!(
        signalled.elapsed() < Duration::from_secs(10),
        "SIGINT waited out the turn: {:?}",
        signalled.elapsed()
    );
    let outcome = agent.finish();
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(
        outcome.status.code(),
        Some(SIGNALLED_EXIT_CODE),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome
            .records()
            .iter()
            .any(|record| record["kind"] == "error"
                && record["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("SIGINT"))),
        "stream:\n{}",
        outcome.lines.join("\n")
    );
    assert_eq!(outcome.assistant_text(), [] as [String; 0]);
}

/// An interrupted JSONL turn grows no auto-continue note line: JSONL never
/// auto-continues, so there is nothing to pause.
#[test]
fn a_jsonl_interrupt_emits_no_auto_continue_note() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "never delivered"}, "stall_ms": 30_000}]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("stall please"));
    agent.wait_for("running", |record| {
        record["kind"] == "status" && record["phase"] == "running"
    });
    wait_for_provider_dispatch(&fixture);
    agent.send(&json!({"v": 1, "kind": "interrupt"}));
    agent.send(&quit_frame());
    let outcome = agent.finish();
    let notes: Vec<_> = outcome
        .lines
        .iter()
        .filter(|line| line.contains("auto-continue paused"))
        .collect();
    assert!(
        notes.is_empty(),
        "JSONL gained {notes:?}; stream:\n{}",
        outcome.lines.join("\n")
    );
}

/// A turn a graceful stop can actually finish, unlike [`stalled_answer`].
///
/// The signal watcher gives the face one second to stop cleanly before it
/// exits hard. `stalled_answer` stalls for three and repeats forever, so its
/// graceful path can never win and every signal on it is a hard exit. This one
/// stalls long enough for the signal to land inside the provider operation and
/// short enough that the stop completes, which is the path a live route takes.
fn interruptible_answer() -> Value {
    json!({
        "version": 1,
        "steps": [
            {"output": {"text": "the interruptible answer"}, "stall_ms": 800}
        ]
    })
}

/// A session stopped by a signal it could act on reopens and carries on.
///
/// This is the graceful half. The turn ends inside the stop deadline, so the
/// session is left in a state a plain reopen accepts, and the next process
/// prompts it without ceremony.
#[test]
fn a_jsonl_session_gracefully_stopped_by_a_signal_reopens_and_carries_on() {
    let fixture = Fixture::new(&interruptible_answer());
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
    let session = hello["session_id"].as_str().expect("session id").to_owned();
    agent.send(&prompt_frame("answer soon"));
    agent.wait_for("running", |record| {
        record["kind"] == "status" && record["phase"] == "running"
    });
    wait_for_provider_dispatch(&fixture);
    signal(agent.id(), SIGTERM);
    agent.close_stdin();
    let stopped = agent.finish();
    stopped.assert_exactly_one_terminal_record();

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "resumed after the graceful stop"}}]
    }));
    let mut resumed = AgentProcess::start(&fixture, &["--session", &session]);
    resumed.wait_for("ready", |record| record["kind"] == "ready");
    resumed.send(&prompt_frame("carry on"));
    resumed.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    resumed.send(&quit_frame());
    let outcome = resumed.finish();
    assert!(
        outcome.status.success(),
        "a gracefully stopped session must reopen; stderr:\n{}",
        outcome.stderr
    );
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(outcome.session_id(), session);
    assert_eq!(
        outcome.assistant_text(),
        vec!["resumed after the graceful stop".to_owned()]
    );
}

/// The operator line a recovered interrupted turn prints, once.
const RECOVERY_NOTE: &str = "recovered an interrupted turn";

/// The face wording that identifies each abandonment claim, for the negative
/// half of a note assertion.
///
/// **The positive assertion is the weaker half.** An arm that checks only that
/// a running-tool cut says "changed this workspace" still passes when a
/// *different* cut borrows that warning, because each arm is happily asserting
/// its own presence. What catches a borrowed warning is asserting that the
/// other claims are absent. tools-fix nearly shipped exactly that bug, and the
/// harness would have certified it.
///
/// These are the face's paraphrases, not the canonical history texts, because
/// this is what a user reads. The two address different readers on purpose: the
/// canonical note goes into history and is read by the model next turn, so it
/// says what is and is not part of that history; the face note goes to an
/// operator, so it says what to do. The canonical wordings and their unit pins
/// live in `runtime/durable/turn_recovery.rs`
/// (`each_abandonment_claim_says_only_what_is_true_of_it`); the face strings
/// are in `hosted/src/runtime/host_loop/open.rs`.
///
/// **Each of these is a discriminator, which is stricter than "a phrase from
/// the claim".** "nothing from it was kept" appears in *both* the running-tool
/// and model-request notes, so asserting its absence to tell those two apart is
/// an assertion that fails on correct output. I wrote exactly that and
/// tools-fix caught it.
///
/// **Do not replace these with constants shared with the product.** It is the
/// obvious tidy-up and it destroys the test. An assertion against the same
/// constant the product emits cannot catch a *wrong* constant, only a missing
/// one, and it agrees with itself while doing so. The bug this file now guards
/// against was a real string saying a false thing: the model-request warning
/// emitted for a cut where nothing had started. A shared-constant test passes
/// that happily.
///
/// The duplication is what makes the assertion evidence. Independently written
/// prose, checked against an independently recorded classification
/// (`assert_abandonment_classification`), is two witnesses. One shared constant
/// is none. The cost is that these drift when the product's wording changes;
/// that cost is the feature, and the owner of those strings publishes the diff
/// when they move.
const RUNNING_TOOL_ONLY: [&str; 2] = ["changed this workspace", "check the workspace"];
const MODEL_REQUEST_ONLY: [&str; 1] = ["model request may have been processed and billed"];
/// The third claim's discriminator. No case here reaches a cut that produces
/// it; it is named so an absence assertion can be complete.
const NOTHING_STARTED_ONLY: [&str; 1] = ["last round"];

/// Asserts the durable classification behind the note, which does not depend on
/// prose at all.
///
/// `turn.abandoned` carries the `claim` and its `cause`. That is the
/// classification itself rather than its rendering, so it does not move when
/// either wording is edited, and it is the assertion to reach for first. The
/// note assertions above check that the *rendering* matches this
/// classification; this checks the classification.
fn assert_abandonment_classification(fixture: &Fixture, claim: &str, cause: &str) {
    let directory = plak::home::paths::PlakPaths::explicit(fixture.home().clone()).sessions_dir();
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(&directory) else {
        panic!("the session log directory is unreadable");
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in body.lines() {
            if let Ok(record) = serde_json::from_str::<Value>(line)
                && record["kind"] == "turn.abandoned"
            {
                found.push(record);
            }
        }
    }
    assert_eq!(
        found.len(),
        1,
        "expected exactly one turn.abandoned record, saw {}",
        found.len()
    );
    let record = &found[0];
    assert_eq!(
        record["claim"], claim,
        "turn.abandoned classified its claim wrongly: {record}"
    );
    assert_eq!(
        record["cause"], cause,
        "turn.abandoned classified its cause wrongly: {record}"
    );
}

/// Asserts a recovery note makes exactly one claim: `present`, and not `absent`.
fn assert_note_claims_only(note: &str, present: &[&str], absent: &[&str], face: &str) {
    for expected in present {
        assert!(
            note.contains(expected),
            "the {face} note does not say {expected:?}; note was:\n{note}"
        );
    }
    for borrowed in absent {
        assert!(
            !note.contains(borrowed),
            "the {face} note borrowed another cut's warning, {borrowed:?}; note was:\n{note}"
        );
    }
}

/// Reopens `session` until the branch lease from the dead process has gone.
///
/// A dead process's branch lease outlives it, and an open that would abandon
/// the turn but cannot acquire the branch is refused until the lease expires.
/// That refusal is deliberate: two processes on one branch is what leases
/// exist to prevent, and it is what keeps a crashed session usable rather than
/// broken. Measured on this machine the lease is already gone by the first
/// attempt for both signals, so this loop is tolerance rather than a wait.
/// Every record kind the session log holds, with counts.
fn session_log_kinds(fixture: &Fixture) -> Vec<String> {
    let directory = plak::home::paths::PlakPaths::explicit(fixture.home().clone()).sessions_dir();
    let mut kinds = Vec::new();
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return kinds;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in body.lines() {
            if let Ok(record) = serde_json::from_str::<Value>(line)
                && let Some(kind) = record["kind"].as_str()
            {
                kinds.push(kind.to_owned());
            }
        }
    }
    kinds
}

/// Asserts the reopened session recovered, said so once, and works.
fn assert_recovered_and_usable(fixture: &Fixture, mut agent: AgentProcess) {
    let note = agent.wait_for("the recovery note", |record| {
        record["kind"] == "status"
            && record["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains(RECOVERY_NOTE))
    });
    let detail = note["detail"].as_str().expect("a note detail");
    // A turn cut inside its model request. It must make the model-request
    // claim and neither of the other two: nothing ran, so nothing can have
    // changed the workspace, and telling an operator to go and look would be a
    // false alarm.
    assert_note_claims_only(
        detail,
        &MODEL_REQUEST_ONLY,
        &[
            RUNNING_TOOL_ONLY[0],
            RUNNING_TOOL_ONLY[1],
            NOTHING_STARTED_ONLY[0],
        ],
        "model-request",
    );
    assert_abandonment_classification(fixture, "model_request", "process_lost");
    agent.send(&prompt_frame("carry on after the recovery"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(
        outcome.assistant_text(),
        vec!["recovered and answered".to_owned()]
    );

    // Said once, not on every turn afterwards.
    let notes = outcome
        .records()
        .iter()
        .filter_map(|record| record["detail"].as_str())
        .filter(|detail| detail.contains(RECOVERY_NOTE))
        .count();
    assert_eq!(notes, 1, "the recovery note must be said once, saw {notes}");

    // And the abandonment is a durable fact, not just an operator line.
    let kinds = session_log_kinds(fixture);
    assert!(
        kinds.iter().any(|kind| kind == "turn.abandoned"),
        "the session log has no turn.abandoned record; kinds were {kinds:?}"
    );
}

/// The script a recovered session answers with.
fn recovery_answer_script() -> Value {
    json!({
        "version": 1,
        "steps": [{"output": {"text": "recovered and answered"}}]
    })
}

/// A session stopped past the deadline recovers its interrupted turn on reopen.
///
/// The turn was cut with an in-flight model request whose outcome was never
/// recorded. A plain reopen now abandons that request, writes one system item
/// saying so, and opens. The note is the operator's only evidence that a
/// provider call may have been charged for output nobody kept, so it is
/// asserted in full rather than by presence.
#[test]
fn a_jsonl_session_stopped_past_the_deadline_recovers_its_interrupted_turn() {
    let fixture = Fixture::new(&stalled_answer());
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
    let session = hello["session_id"].as_str().expect("session id").to_owned();
    agent.send(&prompt_frame("stall please"));
    agent.wait_for("running", |record| {
        record["kind"] == "status" && record["phase"] == "running"
    });
    wait_for_provider_dispatch(&fixture);
    signal(agent.id(), SIGTERM);
    agent.close_stdin();
    let stopped = agent.finish();
    stopped.assert_exactly_one_terminal_record();
    assert_eq!(
        stopped.status.code(),
        Some(SIGNALLED_EXIT_CODE),
        "a stall longer than the stop deadline must exit hard; stderr:\n{}",
        stopped.stderr
    );

    fixture.write_script(&recovery_answer_script());
    let reopened = reopen_until_the_lease_clears(&fixture, &session);
    assert_recovered_and_usable(&fixture, reopened);
}

/// The same recovery after `SIGKILL`, which no handler can soften.
///
/// A killed process writes no terminal record at all, so unlike every other
/// signal case there is nothing to assert about the stopped stream except that
/// it ended. What must survive is the session.
#[test]
fn a_jsonl_session_killed_mid_turn_recovers_its_interrupted_turn() {
    let fixture = Fixture::new(&stalled_answer());
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
    let session = hello["session_id"].as_str().expect("session id").to_owned();
    agent.send(&prompt_frame("stall please"));
    agent.wait_for("running", |record| {
        record["kind"] == "status" && record["phase"] == "running"
    });
    wait_for_provider_dispatch(&fixture);
    signal(agent.id(), SIGKILL);
    agent.close_stdin();
    let killed = agent.finish();
    assert!(
        killed.kinds().iter().all(|kind| kind != "bye"),
        "a killed process cannot write a terminal record; stream was:\n{}",
        killed.lines.join("\n")
    );

    fixture.write_script(&recovery_answer_script());
    let reopened = reopen_until_the_lease_clears(&fixture, &session);
    assert_recovered_and_usable(&fixture, reopened);
}

/// A queued prompt survives SIGKILL without retaining its old JSONL command
/// origin. Reopening leaves it paused until the new connection explicitly
/// sends `resume_queue`; the original prompt is never replayed.
#[test]
fn a_crash_restored_jsonl_queue_resumes_only_after_resume_queue() {
    let fixture = Fixture::new(&stalled_answer());
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
    let session = hello["session_id"].as_str().expect("session id").to_owned();
    agent.send(&prompt_frame("stall before the crash"));
    agent.wait_for("running", |record| {
        record["kind"] == "status" && record["phase"] == "running"
    });
    wait_for_provider_dispatch(&fixture);
    agent.send(
        &json!({"v": 1, "kind": "prompt", "id": "old-queue-command", "text": "saved follow-up"}),
    );
    agent.wait_for("durable queued follow-up", |record| {
        record["kind"] == "queue"
            && record["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["text"] == "saved follow-up"))
    });
    signal(agent.id(), SIGKILL);
    agent.close_stdin();
    let killed = agent.finish();
    assert!(!killed.kinds().iter().any(|kind| kind == "bye"));

    fixture.write_script(&recovery_answer_script());
    let mut reopened = reopen_until_the_lease_clears(&fixture, &session);
    reopened.wait_for("restored queue remains paused", |record| {
        record["kind"] == "queue"
            && record["drain"] == "restored_paused"
            && record["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["text"] == "saved follow-up"))
    });
    let evidence = fixture.root.join("evidence.jsonl");
    let dispatched = || {
        plakous_offline_crash_harness::evidence::read_evidence(&evidence)
            .expect("evidence")
            .iter()
            .filter(|row| {
                row.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count()
    };
    assert_eq!(dispatched(), 1, "reopen must not dispatch the saved prompt");
    reopened.send(&json!({"v": 1, "kind": "resume_queue", "id": "resume"}));
    reopened.wait_for("drain resumed", |record| {
        record["kind"] == "queue" && record["drain"] == "live_auto_drain"
    });
    reopened.wait_for("saved prompt answered", |record| {
        record["kind"] == "assistant_done"
    });
    reopened.send(&quit_frame());
    let completed = reopened.finish();
    assert!(completed.status.success(), "{}", completed.stderr);
    completed.assert_exactly_one_terminal_record();
    assert_eq!(
        completed.assistant_text(),
        vec!["recovered and answered".to_owned()]
    );
    assert_eq!(
        dispatched(),
        2,
        "only the saved prompt may dispatch after resume"
    );
    assert!(
        completed
            .records()
            .iter()
            .filter(|record| record["kind"] == "assistant_done")
            .all(|record| record.get("id").is_none()),
        "a restored queue cannot inherit the prior connection's command id"
    );
}

/// An explicit tree-resume flag on a root-only session remains an ordinary
/// exact session resume on both headless faces.
#[test]
fn absent_tree_resume_flag_works_on_run_and_agent() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": "recovered and answered"}}]
    }));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "seed a root-only session",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let session = first.session_id();
    let second = run_cli(
        &fixture,
        &[
            "run",
            "--session",
            &session,
            "--resume-tree",
            "--",
            "resume without a tree",
        ],
    );
    assert!(second.status.success(), "{}", second.stderr);
    assert!(second.stdout.contains("recovered and answered"));

    let mut agent = AgentProcess::start(&fixture, &["--session", &session, "--resume-tree"]);
    agent.wait_for("root-only agent ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("JSONL also resumes without a tree"));
    agent.wait_for("JSONL answer", |record| record["kind"] == "assistant_done");
    agent.send(&quit_frame());
    let completed = agent.finish();
    assert!(completed.status.success(), "{}", completed.stderr);
    completed.assert_exactly_one_terminal_record();
    assert_eq!(
        completed.assistant_text(),
        vec!["recovered and answered".to_owned()]
    );
}

#[test]
fn corrupt_tree_store_never_counts_as_an_absent_tree() {
    let fixture = Fixture::new(&recovery_answer_script());
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "seed a root-only session",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let session = first.session_id();
    write_private(
        &plak::home::paths::PlakPaths::explicit(fixture.home().clone()).subagents_database(),
        "not a tree database",
    );

    let run = run_cli(
        &fixture,
        &[
            "run",
            "--session",
            &session,
            "--resume-tree",
            "--",
            "must not dispatch",
        ],
    );
    assert!(!run.status.success(), "damaged tree store was accepted");
    assert!(!run.stdout.contains("recovered and answered"));
    let agent = AgentProcess::start(&fixture, &["--session", &session, "--resume-tree"]);
    let rejected = agent.finish();
    assert!(
        !rejected.status.success(),
        "damaged tree store was accepted"
    );
    assert!(!rejected.kinds().iter().any(|kind| kind == "ready"));
    rejected.assert_exactly_one_terminal_record();
}

/// Bounded reproduction probe for the CAS diagnostic that had
/// no counted provider exchange. Each home is new, each completed first turn
/// commits its product session, and the next process reopens that exact row.
/// Run explicitly during qualification; sixty spawned hosts are too costly
/// for the daily offline gate.
#[test]
#[ignore = "bounded 30-home startup/reopen CAS qualification probe"]
fn thirty_isolated_jsonl_startups_and_reopens_preserve_product_revision() {
    const SAMPLES: usize = 30;
    for sample in 0..SAMPLES {
        let fixture = Fixture::new(&json!({
            "version": 1,
            "repeat_last_step": true,
            "steps": [{"output": {"text": "stable answer"}}]
        }));
        let mut first = AgentProcess::start(&fixture, &[]);
        first.wait_for("first ready", |record| record["kind"] == "ready");
        first.send(&prompt_frame("commit this isolated session"));
        first.wait_for("first answer", |record| record["kind"] == "assistant_done");
        first.send(&quit_frame());
        let first = first.finish();
        assert!(first.status.success(), "sample {sample}: {}", first.stderr);
        first.assert_exactly_one_terminal_record();
        let session = first.session_id();

        let mut reopened = AgentProcess::start(&fixture, &["--session", &session]);
        reopened.wait_for("reopened ready", |record| record["kind"] == "ready");
        reopened.send(&prompt_frame("resume this exact session"));
        reopened.wait_for("reopened answer", |record| {
            record["kind"] == "assistant_done"
        });
        reopened.send(&quit_frame());
        let reopened = reopened.finish();
        assert!(
            reopened.status.success(),
            "sample {sample}: {}",
            reopened.stderr
        );
        reopened.assert_exactly_one_terminal_record();
        assert_eq!(reopened.session_id(), session, "sample {sample}");
        assert_eq!(
            reopened.assistant_text(),
            ["stable answer"],
            "sample {sample}"
        );
        assert!(
            reopened
                .records()
                .iter()
                .all(|record| record["kind"] != "error"),
            "sample {sample}: reopened stream contained a product error"
        );
    }
}

/// The `quit` command also saves the product row when no turn has run. This
/// exercises that narrower boundary with and without the home-owned tree gate.
#[test]
#[ignore = "bounded 30-home idle quit/reopen CAS qualification probe"]
fn thirty_isolated_idle_jsonl_quits_and_reopens_preserve_product_revision() {
    const SAMPLES: usize = 30;
    for sample in 0..SAMPLES {
        let fixture = Fixture::new(&recovery_answer_script());
        if sample % 2 == 1 {
            write_private(
                &fixture.home().join("config.toml"),
                "[root]\nagent = \"lead\"\n\n[orchestration]\nsubagents = true\n\n[compaction]\nenabled = true\n",
            );
        }
        let mut first = AgentProcess::start(&fixture, &[]);
        let hello = first.wait_for("idle hello", |record| record["kind"] == "hello");
        first.wait_for("idle ready", |record| record["kind"] == "ready");
        let session = hello["session_id"].as_str().expect("session id").to_owned();
        first.send(&quit_frame());
        let first = first.finish();
        assert!(first.status.success(), "sample {sample}: {}", first.stderr);
        first.assert_exactly_one_terminal_record();

        let mut reopened = AgentProcess::start(&fixture, &["--session", &session]);
        reopened.wait_for("idle reopened ready", |record| record["kind"] == "ready");
        reopened.send(&quit_frame());
        let reopened = reopened.finish();
        assert!(
            reopened.status.success(),
            "sample {sample}: {}",
            reopened.stderr
        );
        reopened.assert_exactly_one_terminal_record();
        assert_eq!(reopened.session_id(), session, "sample {sample}");
        assert!(
            reopened
                .records()
                .iter()
                .all(|record| record["kind"] != "error"),
            "sample {sample}: reopened stream contained a product error: {}",
            reopened.lines.join("\n")
        );
    }
}

/// The CLI face recovers a killed session and says so on stderr, once.
#[test]
fn a_cli_session_killed_mid_turn_recovers_on_the_next_run() {
    let fixture = Fixture::new(&stalled_answer());
    let mut child = spawn_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "stall please",
        ],
    );
    wait_for_provider_dispatch(&fixture);
    signal(child.id(), SIGKILL);
    let killed = wait_with_output(&mut child);
    let session = killed
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("session_id="))
        .expect("the session id was printed before the kill")
        .to_owned();

    fixture.write_script(&recovery_answer_script());
    let deadline = Instant::now() + WAIT;
    let recovered = loop {
        let run = run_cli(&fixture, &["run", "--session", &session, "--", "carry on"]);
        if run.status.success() {
            break run;
        }
        assert!(
            Instant::now() < deadline,
            "the killed session never reopened; stderr was:\n{}",
            run.stderr
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    let notes = recovered
        .stderr
        .lines()
        .filter(|line| line.contains(RECOVERY_NOTE))
        .count();
    assert_eq!(
        notes, 1,
        "the CLI face must say it once; stderr was:\n{}",
        recovered.stderr
    );
    assert!(
        recovered.stdout.contains("recovered and answered"),
        "stdout was: {}",
        recovered.stdout
    );
    let kinds = session_log_kinds(&fixture);
    assert!(
        kinds.iter().any(|kind| kind == "turn.abandoned"),
        "the session log has no turn.abandoned record; kinds were {kinds:?}"
    );
}

/// A session that has already recovered an abandoned turn reopens again.
///
/// Recovery advances the durable head, and the process that recovers has to
/// write that head back into the product record before it leaves. If it does
/// not, every later process is refused with a stale locator — and a stale
/// locator is not a temporal condition, so a caller that retries waits out its
/// whole patience on a refusal that will never change. Live, that is what a
/// session looked like after it had recovered cleanly: the recovery turn
/// answered, and every turn after it was refused.
///
/// This walks three processes, because the fault only appears at the third:
/// the one that crashed, the one that recovered, and the one that comes after.
#[test]
fn a_recovered_session_reopens_for_the_process_after_the_recovery() {
    let fixture = Fixture::new(&stalled_answer());
    let mut child = spawn_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "stall please",
        ],
    );
    wait_for_provider_dispatch(&fixture);
    signal(child.id(), SIGKILL);
    let killed = wait_with_output(&mut child);
    let session = killed
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("session_id="))
        .expect("the session id was printed before the kill")
        .to_owned();

    fixture.write_script(&recovery_answer_script());
    let deadline = Instant::now() + WAIT;
    let recovered = loop {
        let run = run_cli(&fixture, &["run", "--session", &session, "--", "carry on"]);
        if run.status.success() {
            break run;
        }
        assert!(
            Instant::now() < deadline,
            "the killed session never reopened; stderr was:\n{}",
            run.stderr
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(
        recovered.stdout.contains("recovered and answered"),
        "the recovery turn did not answer; stdout was: {}",
        recovered.stdout
    );

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "answered after the recovery"}}]
    }));
    let after = run_cli(&fixture, &["run", "--session", &session, "--", "once more"]);
    assert!(
        after.status.success(),
        "the process after the recovery could not reopen the session; stderr was:\n{}",
        after.stderr
    );
    assert!(
        after.stdout.contains("answered after the recovery"),
        "stdout was: {}",
        after.stdout
    );
    assert!(
        !after.stderr.contains(RECOVERY_NOTE),
        "the recovery note belongs to the turn that recovered, not to the ones \
         after it; stderr was:\n{}",
        after.stderr
    );
}

// ---------------------------------------------------------------------------
// Provider faults
// ---------------------------------------------------------------------------

fn fault_script(at: &str, extra: &Value) -> Value {
    let mut fault = json!({"at": at, "kind": "unavailable", "message": "scripted route refusal"});
    if let (Some(fault), Some(extra)) = (fault.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            fault.insert(key.clone(), value.clone());
        }
    }
    json!({
        "version": 1,
        "steps": [{"output": {"text": "never reached"}, "fault": fault}]
    })
}

fn assert_fault_leaves_the_session_continuable(at: &str, extra: &Value) {
    let fixture = Fixture::new(&fault_script(at, extra));
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
    let session = hello["session_id"].as_str().expect("session id").to_owned();
    agent.send(&prompt_frame("please fail"));
    agent.wait_for("the reported provider failure", |record| {
        record["kind"] == "error"
    });
    // The face must come back to idle rather than end the stream.
    agent.wait_for("idle after the fault", |record| {
        record["kind"] == "busy" && record["busy"] == false
    });

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "continued after the fault"}}]
    }));
    agent.send(&quit_frame());
    let outcome = agent.finish();
    outcome.assert_exactly_one_terminal_record();

    let mut resumed = AgentProcess::start(&fixture, &["--session", &session]);
    resumed.wait_for("ready", |record| record["kind"] == "ready");
    resumed.send(&prompt_frame("carry on"));
    resumed.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    resumed.send(&quit_frame());
    let continued = resumed.finish();
    assert!(
        continued.status.success(),
        "a {at} fault must leave the session continuable; stderr:\n{}",
        continued.stderr
    );
    assert_eq!(
        continued.assistant_text(),
        vec!["continued after the fault".to_owned()]
    );
}

#[test]
fn a_pre_stream_provider_refusal_leaves_the_session_continuable() {
    assert_fault_leaves_the_session_continuable("pre_stream", &json!({}));
}

#[test]
fn an_in_band_provider_error_leaves_the_session_continuable() {
    assert_fault_leaves_the_session_continuable("in_band", &json!({"after_events": 2}));
}

#[test]
fn a_stalled_provider_operation_that_times_out_leaves_the_session_continuable() {
    assert_fault_leaves_the_session_continuable(
        "stalled",
        &json!({"kind": "timeout", "stall_ms": 250}),
    );
}

// ---------------------------------------------------------------------------
// In-process recovery
// ---------------------------------------------------------------------------

/// The first round is accepted, streams two deltas and then reports an outcome
/// nobody can observe, which fences the turn inside a live process. Every later
/// round answers.
fn fenced_then_answering() -> Value {
    json!({
        "version": 1,
        "steps": [
            {"fault": {"at": "in_band", "kind": "outcome_unknown", "after_events": 2}},
            {"output": {"text": "answered after recovery"}}
        ]
    })
}

fn recover_frame() -> Value {
    json!({"v": 1, "kind": "recover", "id": "r1"})
}

/// Every terminal `turn_lifecycle` record, by turn, so a test can require one
/// per turn across a whole run.
fn terminal_lifecycles(outcome: &AgentOutcome) -> Vec<(String, String)> {
    outcome
        .records()
        .into_iter()
        .filter(|record| {
            record["kind"] == "turn_lifecycle"
                && matches!(
                    record["phase"].as_str(),
                    Some("completed" | "failed" | "cancelled" | "recovery_required")
                )
        })
        .map(|record| {
            (
                record["turn_id"].as_str().unwrap_or_default().to_owned(),
                record["phase"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn assert_one_terminal_per_turn(outcome: &AgentOutcome) {
    let terminals = terminal_lifecycles(outcome);
    let mut turns = terminals.iter().map(|(turn, _)| turn).collect::<Vec<_>>();
    turns.sort();
    turns.dedup();
    assert_eq!(
        turns.len(),
        terminals.len(),
        "a turn published more than one terminal: {terminals:?}"
    );
}

/// JSONL `recover` reopens a session a live turn fenced, emits one
/// must-not-drop `recovered` record naming the claim and the cause, and the
/// next prompt answers. Nothing is replayed and nothing asks for `/clear`.
#[test]
fn a_jsonl_recover_command_recovers_a_fenced_session_in_place() {
    let fixture = Fixture::new(&fenced_then_answering());
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("fence please"));
    agent.wait_for("the fenced terminal", |record| {
        record["kind"] == "turn_lifecycle" && record["phase"] == "recovery_required"
    });
    agent.send(&recover_frame());
    let recovered = agent.wait_for("recovered", |record| record["kind"] == "recovered");
    assert_eq!(recovered["claim"], "model_request", "{recovered}");
    assert_eq!(recovered["cause"], "interrupted", "{recovered}");
    assert_eq!(
        recovered["id"], "r1",
        "correlated with the command: {recovered}"
    );
    agent.send(&prompt_frame("carry on"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_one_terminal_per_turn(&outcome);
    assert_eq!(
        outcome
            .kinds()
            .iter()
            .filter(|kind| *kind == "recovered")
            .count(),
        1
    );
    assert!(
        !outcome.lines.iter().any(|line| line.contains("/clear")),
        "nothing sends the operator to /clear:\n{}",
        outcome.lines.join("\n")
    );
    assert_eq!(
        outcome.assistant_text().last().map(String::as_str),
        Some("answered after recovery")
    );
    assert_abandonment_classification(&fixture, "model_request", "interrupted");
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|record| record["kind"] == "status"
                && record["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains(RECOVERY_NOTE)))
            .count(),
        1,
        "recover emits exactly one operator recovery note"
    );
}

/// A prompt sent straight after the fence recovers first and then answers; the
/// `recovered` record is correlated with that prompt.
#[test]
fn a_jsonl_prompt_after_a_fence_recovers_first_and_then_answers() {
    let fixture = Fixture::new(&fenced_then_answering());
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("fence please"));
    agent.wait_for("the fenced terminal", |record| {
        record["kind"] == "turn_lifecycle" && record["phase"] == "recovery_required"
    });
    agent.send(&json!({"v": 1, "kind": "prompt", "id": "p2", "text": "carry on"}));
    let recovered = agent.wait_for("recovered", |record| record["kind"] == "recovered");
    assert_eq!(recovered["id"], "p2", "{recovered}");
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    assert_one_terminal_per_turn(&outcome);
    assert_eq!(
        outcome.assistant_text().last().map(String::as_str),
        Some("answered after recovery")
    );
}

/// `plak run` on a fenced turn exits 5 and says the session recovers itself;
/// the next `run --session` recovers it and answers, and `plak session
/// recover` then has nothing left to do.
#[test]
fn a_fenced_cli_run_recovers_on_the_next_run_and_session_recover_is_idempotent() {
    let fixture = Fixture::new(&fenced_then_answering());
    let fenced = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "fence please",
        ],
    );
    assert_eq!(fenced.status.code(), Some(5), "stderr:\n{}", fenced.stderr);
    assert!(
        fenced
            .stderr
            .contains("recovers automatically the next time it is used"),
        "stderr:\n{}",
        fenced.stderr
    );
    assert!(
        !fenced.stderr.contains("/clear"),
        "stderr:\n{}",
        fenced.stderr
    );
    let session = fenced.session_id();
    // A new process replays its script from the first step.
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "answered after recovery"}}]
    }));

    let next = run_cli(&fixture, &["run", "--session", &session, "--", "carry on"]);
    assert!(next.status.success(), "stderr:\n{}", next.stderr);
    assert_eq!(
        next.stderr
            .lines()
            .filter(|line| line.contains(RECOVERY_NOTE))
            .count(),
        1,
        "stderr:\n{}",
        next.stderr
    );
    assert!(
        next.stdout.contains("answered after recovery"),
        "{}",
        next.stdout
    );

    let recover = run_cli(&fixture, &["session", "recover", &session]);
    assert!(recover.status.success(), "stderr:\n{}", recover.stderr);
    assert!(
        recover.stdout.contains("nothing to recover"),
        "stdout:\n{}\nstderr:\n{}",
        recover.stdout,
        recover.stderr
    );
}

/// `plak session recover` recovers a fenced session on its own, without
/// running a turn.
#[test]
fn plak_session_recover_recovers_a_fenced_session_without_a_turn() {
    let fixture = Fixture::new(&fenced_then_answering());
    let fenced = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "fence please",
        ],
    );
    assert_eq!(fenced.status.code(), Some(5), "stderr:\n{}", fenced.stderr);
    let session = fenced.session_id();
    let recover = run_cli(&fixture, &["session", "recover", &session]);
    assert!(recover.status.success(), "stderr:\n{}", recover.stderr);
    assert!(
        recover.stdout.contains(RECOVERY_NOTE),
        "stdout:\n{}\nstderr:\n{}",
        recover.stdout,
        recover.stderr
    );
    let evidence = std::fs::read_to_string(fixture.script().with_file_name("evidence.jsonl"))
        .unwrap_or_default();
    let dispatches = evidence
        .lines()
        .filter(|line| line.contains("provider_dispatched"))
        .count();
    assert_eq!(dispatches, 1, "recovery dispatched nothing:\n{evidence}");
}

// ---------------------------------------------------------------------------
// Slow reader
// ---------------------------------------------------------------------------

/// A JSONL client that stops reading must not make the writer declare failure.
///
/// The pipe fills while the reader sleeps; a writer that treats a blocked pipe
/// as a broken one ends the stream early, which is exactly what a real slow
/// consumer saw. Volume comes from assistant text, not from tool results: the
/// face clips a tool observation, so a large tool body never reaches stdout,
/// while assistant text is published verbatim. Three answers of 100 kB is
/// several times any pipe buffer. Budget lane: this asserts a wall-clock stall.
#[test]
fn a_slow_jsonl_reader_does_not_make_the_writer_fail_the_stream() {
    let answer = "z".repeat(100_000);
    let fixture = Fixture::new(&json!({
        "version": 1,
        "repeat_last_step": true,
        // A tool result on the way, so the turn is a real one and not just a
        // text dump, and the clipped observation is exercised too.
        "tool_result_bytes": 120_000,
        "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": answer}}
        ]
    }));
    let mut agent = AgentProcess::start_with_drain(&fixture, &[], false);
    let mut stdout = agent.child.stdout.take().expect("stdout pipe");
    for ordinal in 0..3 {
        agent.send(&prompt_frame(&format!(
            "produce the large answer {ordinal}"
        )));
    }
    // Deliberately do not drain for several seconds.
    std::thread::sleep(Duration::from_secs(5));
    let drained = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&drained);
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(&mut stdout).lines().map_while(Result::ok) {
            sink.lock().unwrap().push(line);
        }
    });
    let deadline = Instant::now() + WAIT;
    loop {
        let answered = drained
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.len() > 100_000)
            .count();
        if answered >= 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the slow reader received {answered} of 3 large answers; lines so far: {}",
            drained.lock().unwrap().len()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let buffered: usize = drained.lock().unwrap().iter().map(String::len).sum();
    assert!(
        buffered > 256 * 1024,
        "the run never exceeded a pipe buffer, so it did not test a blocked writer: {buffered} bytes"
    );
    agent.send(&quit_frame());
    agent.close_stdin();
    let _ = agent.child.wait();
    let _ = reader.join();
    let lines = drained.lock().unwrap().clone();
    let byes = lines
        .iter()
        .filter(|line| line.contains("\"kind\":\"bye\""))
        .count();
    assert_eq!(
        byes, 1,
        "expected exactly one terminal record after a slow read"
    );
    for line in &lines {
        assert!(
            !line.contains("stdout failed") && !line.contains("stream failed"),
            "the writer declared failure under a slow reader: {}",
            line.chars().take(200).collect::<String>()
        );
    }
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

/// The growth script: two large tool results either side of the fence.
fn growth_script(fold: &str) -> Value {
    // Every round reports its projected size as the provider input, so the
    // measure anchors and the fence can act (an unanchored estimate never
    // schedules a compaction).
    json!({
        "version": 1,
        "repeat_last_step": true,
        "maximum_projected_input": 20_000,
        "scheduled_compaction": true,
        "content_sensitive_projection": true,
        "tool_result_bytes": 5_800,
        "fold": fold,
        "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls", "usage": {}},
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls", "usage": {}},
            {"output": {"text": "the compacted answer"}, "usage": {}}
        ]
    })
}

#[test]
fn a_no_eligible_manual_compact_does_not_block_the_next_compact() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "the later prompt is answered"}}]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
    let session = hello["session_id"].as_str().unwrap().to_owned();
    agent.wait_for("ready", |record| record["kind"] == "ready");
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    for id in ["compact-1", "compact-2"] {
        agent.send(&json!({"v": 1, "kind": "compact", "id": id}));
        agent.wait_for("the no-eligible-range outcome", |record| {
            record["id"] == id
                && record["kind"] == "status"
                && record["phase"] == "note"
                && record["detail"].as_str().is_some_and(|detail| {
                    detail.contains("nothing in this conversation can be compacted yet")
                })
        });
        let pending: i64 = rusqlite::Connection::open(paths.product_database())
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM workflow_requests WHERE session_id = ?1",
                [&session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0, "{id} leaves no pending workflow request");
    }
    agent.send(&prompt_frame("continue"));
    agent.wait_for_completed_turns(1);
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    assert_eq!(
        outcome.assistant_text(),
        vec!["the later prompt is answered"]
    );
    let dispatches =
        plakous_offline_crash_harness::evidence::read_evidence(fixture.root.join("evidence.jsonl"))
            .unwrap()
            .iter()
            .filter(|row| {
                row.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count();
    assert_eq!(dispatches, 1, "neither no-op compact was paid");
}

fn compact_frame() -> Value {
    json!({"v": 1, "kind": "compact"})
}

/// A turn that crosses the context fence compacts mid-turn and still answers.
///
/// This route has no semantic compaction facet, so the between-rounds fence
/// takes the offline placeholder rung: the older tool result is replaced with a
/// placeholder and the projection returns under the ceiling. The note names
/// both, so a build that stopped replacing oldest results cannot pass by answering.
#[test]
fn a_turn_that_crosses_the_context_fence_replaces_oldest_results_mid_turn_and_still_answers() {
    let fixture = Fixture::new(&growth_script("accept"));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("grow the context"));
    let replaced = agent.wait_for("the mid-turn Compact row", |record| {
        record["kind"] == "tool"
            && record["name"] == "compact"
            && record["state"] != "running"
            && record["subject"].as_str().is_some_and(|subject| {
                subject.contains('→') || subject.contains("->") || subject.contains('k')
            })
    });
    assert!(
        replaced["activity_kind"] == "compact",
        "compaction is a Compact tool row, not a status essay: {replaced}"
    );
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(
        outcome.assistant_text(),
        vec!["the compacted answer".to_owned()],
        "stream:\n{}",
        outcome.lines.join("\n")
    );
}

/// Keeps an older intent foldable without an automatic compact consuming it
/// before the explicit command, then proves the source cursor continues.
fn compacted_stream(fold: &str) -> AgentOutcome {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "maximum_projected_input": 20_000,
        "content_sensitive_projection": true,
        "fold": fold,
        "steps": [
            {"output": {"text": "the first answer"}, "usage": {}},
            {"output": {"text": "the second answer"}, "usage": {}},
            {"output": {"text": "the compacted answer"}, "usage": {}}
        ]
    }));
    write_private(
        &fixture.home().join("config.toml"),
        "[root]\nagent = \"lead\"\n\n[orchestration]\nsubagents = false\n\n[compaction]\nenabled = false\n",
    );
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("answer the first intent"));
    agent.wait_for("the first answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the first answer"
    });
    agent.wait_for_completed_turns(1);
    agent.send(&prompt_frame("answer the second intent"));
    agent.wait_for("the second answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the second answer"
    });
    agent.wait_for_completed_turns(2);
    agent.send(&compact_frame());
    let compact = agent.wait_for("the compaction outcome", |record| {
        record["kind"] == "tool"
            && record["activity_kind"] == "compact"
            && matches!(record["state"].as_str(), Some("done" | "failed"))
    });
    assert_eq!(compact["state"], "done", "the fold committed: {compact}");

    // The session must still take a prompt afterwards.
    agent.send(&prompt_frame("carry on after the compaction"));
    agent.wait_for("the follow-up answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the compacted answer"
    });
    agent.wait_for_completed_turns(3);
    agent.send(&quit_frame());
    let outcome = agent.finish();
    let evidence =
        plakous_offline_crash_harness::evidence::read_evidence(fixture.root.join("evidence.jsonl"))
            .expect("physical dispatch evidence");
    let rounds = evidence
        .iter()
        .filter(|record| {
            record.kind == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
        })
        .map(|record| record.round)
        .collect::<Vec<_>>();
    assert_eq!(
        rounds,
        vec![Some(1), Some(2), Some(1), Some(3)],
        "one independently dispatched fold between the source turns"
    );
    outcome
}

/// Refuses a compaction outcome that reports a failure.
///
/// `detail.contains("compact")` is true of the *failure* text as well as the
/// success text, so an assertion written that way passes on a compaction that
/// did not happen. Driving `/compact` on the terminal shows this,
/// where the refusal is painted rather than buried in a status detail.
fn assert_compaction_did_not_fail(outcome: &AgentOutcome) {
    let failures = compaction_details(outcome)
        .into_iter()
        .filter(|detail| detail.contains("failed") || detail.contains("not exact"))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "the compaction reported a failure: {failures:?}\n{}",
        outcome.lines.join("\n")
    );
}

/// An exact scripted summary binding commits the fold and the session continues.
#[test]
fn an_explicit_compaction_whose_fold_is_accepted_commits_and_continues() {
    let outcome = compacted_stream("accept");
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_compaction_did_not_fail(&outcome);
    let records = outcome.records();
    assert!(
        records
            .iter()
            .any(|record| record["activity_kind"] == "compact" && record["state"] == "done"),
        "no committed Compact row was published:\n{}",
        outcome.lines.join("\n")
    );
    assert_eq!(
        outcome.assistant_text(),
        vec![
            "the first answer".to_owned(),
            "the second answer".to_owned(),
            "the compacted answer".to_owned()
        ],
        "the source script continues after the summary without advancing its cursor"
    );
}

/// A refused fold falls to the offline placeholder rung rather than failing.
///
/// The scripted model refuses preparation of the summary request only. The
/// ladder must not report that as a turn failure: it must replace instead, and
/// the session must stay usable.
#[test]
fn an_explicit_compaction_whose_fold_is_refused_falls_to_placeholders_and_continues() {
    let fixture = Fixture::new(&growth_script("refuse"));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("grow the context"));
    agent.wait_for("placeholder Compact row", |record| {
        record["kind"] == "tool"
            && record["name"] == "compact"
            && record["state"] == "done"
            && record["activity_kind"] == "compact"
    });
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&prompt_frame("carry on after the compaction"));
    agent.wait_for_completed_turns(2);
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert!(
        !outcome
            .lines
            .iter()
            .any(|line| line.contains("recovery") || line.contains("continuation is blocked")),
        "a refused fold must not strand the session:\n{}",
        outcome.lines.join("\n")
    );
}

/// A provider that refuses a round's input as over its window gets a
/// compaction and the same round again, as a new operation, and the turn
/// answers. Before, the turn failed and the host compacted afterwards, so the
/// operator had to send the prompt a second time.
#[test]
fn a_provider_context_overflow_compacts_and_answers_the_same_turn() {
    let first_answer = format!("the first answer\n{}", "detail ".repeat(800));
    let fixture = Fixture::new(&json!({
        "version": 1,
        "scheduled_compaction": true,
        "content_sensitive_projection": true,
        "steps": [
            {"output": {"text": first_answer.clone()}},
            {"fault": {"at": "pre_stream", "kind": "context_overflow",
                       "message": "prompt is too long: 270000 tokens > 256000 maximum"}},
            {"output": {"text": "Summary of earlier work:\nthe first answer was given"}},
            {"output": {"text": "the recovered answer"}}
        ]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("the first prompt"));
    agent.wait_for("the first answer", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&prompt_frame("the prompt the provider refuses"));
    agent.wait_for("the recovered answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the recovered answer"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(
        outcome.assistant_text(),
        vec![first_answer, "the recovered answer".to_owned()],
        "stream:\n{}",
        outcome.lines.join("\n")
    );
    assert!(
        !outcome
            .lines
            .iter()
            .any(|line| line.contains("turn failed")),
        "the refused round must not fail the turn:\n{}",
        outcome.lines.join("\n")
    );
}

/// A paid fold whose successor local admission refuses is a definite failed
/// compaction, never a recovery fence. The summary here is larger than the
/// window, so the successor cannot be prepared; that used to end the turn
/// `RecoveryRequired` and pause every later prompt. The free rung has nothing
/// to replace, the turn stops at the context limit, and the session continues.
#[test]
fn a_fold_whose_successor_admission_refuses_fails_without_fencing_the_session() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "maximum_projected_input": 20_000,
        "scheduled_compaction": true,
        "content_sensitive_projection": true,
        "steps": [
            {"output": {"text": "ok"}, "usage": {}},
            {"fault": {"at": "pre_stream", "kind": "context_overflow",
                       "message": "prompt is too long: 270000 tokens > 256000 maximum"}},
            {"output": {"text": format!("Summary of earlier work:\n{}", "s".repeat(120_000))}},
            {"output": {"text": "the follow-up answer"}, "usage": {}}
        ]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("hi"));
    agent.wait_for_completed_turns(1);
    agent.send(&prompt_frame("the second prompt"));
    let second = agent.wait_for("the second turn's terminal", |record| {
        record["kind"] == "turn_lifecycle"
            && matches!(
                record["phase"].as_str(),
                Some("failed" | "cancelled" | "recovery_required")
            )
            || (record["kind"] == "assistant" && record["text"] == "the follow-up answer")
    });
    assert_eq!(
        second["phase"],
        "failed",
        "a refused successor must not fence the session:\n{}",
        agent.snapshot().join("\n")
    );
    agent.send(&prompt_frame("carry on"));
    agent.wait_for("the follow-up answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the follow-up answer"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert!(
        !outcome
            .records()
            .iter()
            .any(|record| record["activity_kind"] == "compact" && record["state"] == "done"),
        "no compaction may commit:\n{}",
        outcome.lines.join("\n")
    );
}

/// A fold whose summary is larger than the older turn it replaced is not
/// published. Spec 01 requires a strictly smaller semantic input on the same
/// measurement basis. The fold fails definitely, the free rung has nothing to
/// replace, the turn stops at the context limit, and the session continues.
#[test]
fn a_fold_summary_larger_than_the_folded_turn_is_not_published() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "scheduled_compaction": true,
        "content_sensitive_projection": true,
        "steps": [
            {"output": {"text": "ok"}, "usage": {}},
            {"fault": {"at": "pre_stream", "kind": "context_overflow",
                       "message": "prompt is too long: 270000 tokens > 256000 maximum"}},
            {"output": {"text": format!("Summary of earlier work:\n{}", "s".repeat(12_000))}},
            {"output": {"text": "the follow-up answer"}, "usage": {}}
        ]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("hi"));
    agent.wait_for_completed_turns(1);
    agent.send(&prompt_frame("the second prompt"));
    let second = agent.wait_for("the second turn's terminal", |record| {
        record["kind"] == "turn_lifecycle"
            && matches!(
                record["phase"].as_str(),
                Some("failed" | "cancelled" | "recovery_required")
            )
            || (record["kind"] == "assistant" && record["text"] == "the follow-up answer")
    });
    assert_eq!(
        second["phase"],
        "failed",
        "the growing fold must not publish:\n{}",
        agent.snapshot().join("\n")
    );
    agent.send(&prompt_frame("carry on"));
    agent.wait_for("the follow-up answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the follow-up answer"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert!(
        !outcome
            .records()
            .iter()
            .any(|record| record["activity_kind"] == "compact" && record["state"] == "done"),
        "no compaction may commit:\n{}",
        outcome.lines.join("\n")
    );
}

/// The live long-session `overflow` shape up to its continuation: one
/// oversized batch is clipped, the turn commits a scheduled compaction
/// mid-turn and completes. Returns the fixture (which owns the home) and the
/// running agent with `continuation` queued behind that turn.
fn overflow_turn_then(continuation: &str) -> (Fixture, AgentProcess) {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "maximum_projected_input": 40_000,
        "scheduled_compaction": true,
        "content_sensitive_projection": true,
        "tool_result_bytes": 60_000,
        "steps": [
            {"output": {"text": "a".repeat(3_000)}, "usage": {}},
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}},
             "finish": "tool_calls", "usage": {}},
            // The mid-turn fold is a round of this same script.
            {"output": {"text": "Summary of earlier work:\nthe first answer was given"}},
            {"output": {"text": "the batch is read"}, "usage": {}},
            {"output": {"text": "the continuation answer"}, "usage": {}}
        ]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("the first prompt"));
    agent.wait_for_completed_turns(1);
    agent.send(&prompt_frame("read the oversized batch"));
    agent.wait_for_completed_turns(2);
    assert!(
        agent
            .snapshot()
            .iter()
            .any(|line| line.contains("the batch is read")),
        "the overflow turn answers after its fold:\n{}",
        agent.snapshot().join("\n")
    );
    agent.send(&prompt_frame(continuation));
    (fixture, agent)
}

/// A paused queue is the defect both overflow continuations used to hit.
fn assert_queue_not_parked(outcome: &AgentOutcome) {
    assert!(
        !outcome
            .records()
            .iter()
            .any(|record| record["kind"] == "error"
                && record["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("drain is paused")
                        || message.contains("remains queued"))),
        "the continuation must not be parked:\n{}",
        outcome.lines.join("\n")
    );
}

/// The next ordinary prompt after that overflow turn runs. It used to be
/// refused: the idle fence scheduled another pass, the placeholder rung would
/// not touch the batch the final reply had already answered, and "nothing can
/// be compacted" paused the queue.
#[test]
fn a_prompt_after_an_overflow_turn_with_a_committed_compaction_runs() {
    let (_fixture, mut agent) = overflow_turn_then("carry on after the overflow");
    agent.wait_for("the continuation answer or a refusal", |record| {
        (record["kind"] == "assistant" && record["text"] == "the continuation answer")
            || record["kind"] == "error"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_queue_not_parked(&outcome);
    let records = outcome.records();
    let committed = records
        .iter()
        .filter(|record| record["activity_kind"] == "compact" && record["state"] == "done")
        .count();
    assert!(
        committed >= 2,
        "the overflow turn and the idle fence each commit a compaction:\n{}",
        outcome.lines.join("\n")
    );
    assert!(
        !records.iter().any(|record| record["kind"] == "error"),
        "the continuation must not be refused:\n{}",
        outcome.lines.join("\n")
    );
    assert_eq!(
        outcome.assistant_text().last().map(String::as_str),
        Some("the continuation answer"),
        "stream:\n{}",
        outcome.lines.join("\n")
    );
}

/// A continuation that cannot fit the window even after the idle fence has
/// replaced everything it can is sent, and the turn withholds it with the
/// resend-shorter remedy. No fold is paid for it (the retained tail alone is
/// over the window, so no summary could help), the session is not fenced, and
/// the next prompt runs on the same script.
#[test]
fn an_over_window_prompt_after_an_overflow_turn_is_withheld_and_the_session_continues() {
    let (_fixture, mut agent) = overflow_turn_then(&"p".repeat(45_000));
    // The first two turns completed, so any other terminal is the third's.
    let third = agent.wait_for("the third turn's terminal", |record| {
        record["kind"] == "turn_lifecycle"
            && matches!(
                record["phase"].as_str(),
                Some("failed" | "cancelled" | "recovery_required")
            )
    });
    assert_eq!(
        third["phase"],
        "failed",
        "a prompt refused locally must not fence the session:\n{}",
        agent.snapshot().join("\n")
    );
    agent.send(&prompt_frame("a short follow-up"));
    agent.wait_for("the follow-up answer or a refusal", |record| {
        (record["kind"] == "assistant" && record["text"] == "the continuation answer")
            || (record["kind"] == "turn_lifecycle"
                && matches!(record["phase"].as_str(), Some("recovery_required")))
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_queue_not_parked(&outcome);
    assert!(
        outcome.lines.iter().any(|line| line.contains("withheld")),
        "the over-window prompt is withheld with the resend remedy:\n{}",
        outcome.lines.join("\n")
    );
    assert!(
        !outcome
            .lines
            .iter()
            .any(|line| line.contains("retrying compaction")),
        "{}",
        outcome.lines.join("\n")
    );
    assert_eq!(
        outcome.assistant_text().last().map(String::as_str),
        Some("the continuation answer"),
        "the follow-up got the script's next step, so no fold was paid:\n{}",
        outcome.lines.join("\n")
    );
}

/// Waits for `answers` completed answers, a parked queue or a failed turn.
fn wait_for_answers_or_park(agent: &AgentProcess, answers: usize) {
    agent.wait_for("an answer, a parked queue or a failed turn", |record| {
        record["kind"] == "assistant_done"
            && agent
                .snapshot()
                .iter()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|record| record["kind"] == "assistant_done")
                .count()
                >= answers
            || (record["kind"] == "error"
                && record["message"].as_str().is_some_and(|message| {
                    message.contains("drain is paused") || message.contains("remains queued")
                }))
            || (record["kind"] == "turn_lifecycle"
                && matches!(
                    record["phase"].as_str(),
                    Some("failed" | "recovery_required")
                ))
    });
}

/// The bulky second turn leaves the session near the window, so the next
/// prompt schedules an idle pass whose only foldable range is the tiny first
/// turn. A summary of it cannot help: the successor would hold the retained
/// bulky turn plus a summary larger than what it replaced. That fold used to be
/// paid, refused (a definite failure) and then park the prompt behind the
/// paid-retry note, so every resume paid for the same refused fold. The idle
/// gate now measures the retained tail with the pending prompt and takes the
/// free rung instead, and the prompt goes to its turn.
#[test]
fn an_idle_fold_that_cannot_help_is_not_paid_and_does_not_park_the_prompt() {
    let summary = format!("Summary of earlier work:\n{}", "s".repeat(5_000));
    let fixture = Fixture::new(&json!({
        "version": 1,
        "maximum_projected_input": 60_000,
        "scheduled_compaction": true,
        "content_sensitive_projection": true,
        "repeat_last_step": true,
        "steps": [
            {"output": {"text": "ok"}, "usage": {}},
            {"output": {"text": summary}, "usage": {}}
        ]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("hi"));
    agent.wait_for_completed_turns(1);
    agent.send(&prompt_frame(&"p".repeat(50_000)));
    wait_for_answers_or_park(&agent, 2);
    // The bulky turn now crowds the observation reserve, so the next prompt
    // schedules a fold of the tiny first turn, which cannot shrink.
    agent.send(&prompt_frame("carry on"));
    wait_for_answers_or_park(&agent, 3);
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_queue_not_parked(&outcome);
    assert!(
        !outcome.lines.iter().any(|line| {
            line.contains("successor does not independently fit")
                || line.contains("did not prove net input shrink")
        }),
        "no idle fold that cannot help is paid for:\n{}",
        outcome.lines.join("\n")
    );
    assert_eq!(
        outcome
            .records()
            .iter()
            .filter(|record| record["kind"] == "assistant_done")
            .count(),
        3,
        "the prompt is answered:\n{}",
        outcome.lines.join("\n")
    );
    let evidence =
        plakous_offline_crash_harness::evidence::read_evidence(fixture.root.join("evidence.jsonl"))
            .unwrap();
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let database = rusqlite::Connection::open(paths.product_database()).unwrap();
    let compactions: i64 = database
        .query_row(
            "SELECT compaction_count FROM session_views WHERE session_id = ?1",
            [outcome.session_id()],
            |row| row.get(0),
        )
        .unwrap();
    let pending_requests: i64 = database
        .query_row(
            "SELECT COUNT(*) FROM workflow_requests WHERE session_id = ?1",
            [outcome.session_id()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending_requests, 0,
        "no unpaid no-op request blocks later work"
    );
    // A strictly shrinking turn-fence fold after the answer may still be paid.
    assert_eq!(
        evidence
            .iter()
            .filter(|record| record.kind
                == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched)
            .count(),
        3 + usize::try_from(compactions).unwrap(),
        "only answers and committed shrinking folds are paid, never the idle no-op"
    );
}

/// Every status detail a compaction published.
fn compaction_details(outcome: &AgentOutcome) -> Vec<String> {
    outcome
        .records()
        .iter()
        .filter_map(|record| record["detail"].as_str())
        .filter(|detail| detail.contains("compact") || detail.contains("replaced"))
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------------------
// Process sessions
// ---------------------------------------------------------------------------

/// A script whose tools are the real coding packs over a POSIX shell.
///
/// `$session_id` and `$cursor` are filled from the previous process
/// observation, because the run mints the id and a script cannot know it.
fn process_session_script(command: &str, tty: bool, steps_after_start: &Value) -> Value {
    let mut steps = vec![json!({
        "output": {"tool": {"name": "process_session_start", "arguments": {
            "command": command,
            "workdir": ".",
            "login": false,
            "tty": tty,
            "keep_stdin_open": true,
            "yield_time_ms": 1000,
            "max_output_bytes": 8192
        }}},
        "finish": "tool_calls"
    })];
    if let Some(extra) = steps_after_start.as_array() {
        steps.extend(extra.iter().cloned());
    }
    steps.push(json!({"output": {"text": "the process turn is done"}}));
    json!({"version": 1, "process_tools": true, "steps": steps})
}

/// Starting, polling and terminating a real child through the JSONL face.
///
/// This is the only route from a spawned `plak` into the process backend's own
/// spawn and teardown: `process_session_terminate` reaches
/// `JoinedProcessSessions::terminate`, which is what calls
/// `terminate_direct_child` for a pipe-backed child.
#[test]
fn a_process_session_starts_polls_and_terminates_through_the_jsonl_face() {
    let fixture = Fixture::new(&process_session_script(
        "sleep 30",
        false,
        &json!([
            {"output": {"tool": {"name": "process_session_poll", "arguments": {
                "session_id": "$session_id",
                "cursor": "$cursor",
                "yield_time_ms": 200,
                "max_output_bytes": 8192,
                "max_output_records": 64
            }}}, "finish": "tool_calls"},
            {"output": {"tool": {"name": "process_session_terminate", "arguments": {
                "session_id": "$session_id"
            }}}, "finish": "tool_calls"}
        ]),
    ));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("run a process session"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    let tools = completed_tool_names(&outcome);
    for expected in [
        "process_session_start",
        "process_session_poll",
        "process_session_terminate",
    ] {
        assert!(
            tools.iter().any(|name| name == expected),
            "{expected} never completed; tools were {tools:?}\n{}",
            outcome.lines.join("\n")
        );
    }
    assert_eq!(
        outcome.assistant_text(),
        vec!["the process turn is done".to_owned()]
    );
}

/// A pty-backed child, torn down when the agent quits with it still live.
///
/// Nothing terminates this one explicitly: quitting drops the session guard,
/// which terminates what is still running, and for a `tty` session that is
/// `terminate_direct_pty_child` rather than the pipe-backed path above.
#[test]
fn a_live_pty_process_session_is_torn_down_when_the_agent_quits() {
    let fixture = Fixture::new(&process_session_script("sleep 30", true, &json!([])));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("start a pty session and leave it running"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    let tools = completed_tool_names(&agent_snapshot(&agent));
    assert!(
        tools.iter().any(|name| name == "process_session_start"),
        "the pty session never started; tools were {tools:?}"
    );
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(
        outcome.status.success(),
        "a quit with a live pty session must still exit cleanly; stderr:\n{}",
        outcome.stderr
    );
    outcome.assert_exactly_one_terminal_record();
}

/// The verifier sees the task container only after `plak run` exits. Exercise
/// that handoff through the real process tool, with a tiny local HTTP server.
/// Promoted processes survive turn end; shutdown still stops their owned group.
/// A new session explicitly detaches from that group on a shared network. Full Linux
/// containment instead isolates loopback, so startup is self-probed there.
/// This does not attribute any archived model's service failure.
#[test]
fn process_tool_http_service_lifetime_at_verifier_handoff() {
    for detached in [false, true] {
        let server = r#"import http.server, pathlib, threading, urllib.request
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"service-ready")
        if self.path == "/stop":
            threading.Thread(target=self.server.shutdown, daemon=True).start()
    def log_message(self, *args):
        pass
server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
worker = threading.Thread(target=server.serve_forever)
worker.start()
expiry = threading.Timer(15, server.shutdown)
expiry.daemon = True
expiry.start()
with urllib.request.urlopen(f"http://127.0.0.1:{server.server_port}/", timeout=2) as response:
    assert response.read() == b"service-ready"
namespace = pathlib.Path("/proc/self/ns/net")
pathlib.Path("service.namespace").write_text(str(namespace.readlink()) if namespace.exists() else "")
pathlib.Path("service.port.tmp").write_text(str(server.server_port)); pathlib.Path("service.port.tmp").replace("service.port")
worker.join()
"#;
        let command = if detached {
            // Wait for the self-probe before the namespace-owning launcher exits.
            "python3 launch.py"
        } else {
            "nohup python3 server.py >service.log 2>&1 </dev/null & wait"
        };
        // Keep the turn open until the host has observed startup. This is a
        // handshake through the shared workspace, not a fixed scheduling gap.
        let probe_gate = json!([{
            "output": {"tool": {"name": "process_session_start", "arguments": {
                "command": "python3 wait.py service.probed",
                "workdir": ".", "login": false, "tty": false,
                "keep_stdin_open": false,
                "yield_time_ms": 10_000, "max_output_bytes": 8192
            }}}, "finish": "tool_calls"
        }]);
        let fixture = Fixture::new(&process_session_script(command, false, &probe_gate));
        std::fs::write(fixture.workspace().join("server.py"), server).expect("server fixture");
        std::fs::write(
            fixture.workspace().join("wait.py"),
            r#"import pathlib, sys, time
deadline = time.monotonic() + 8
while not pathlib.Path(sys.argv[1]).exists():
    assert time.monotonic() < deadline, "fixture handshake expired"
    time.sleep(0.02)
"#,
        )
        .expect("fixture handshake");
        std::fs::write(
            fixture.workspace().join("launch.py"),
            r#"import subprocess
subprocess.Popen(["python3", "server.py"], start_new_session=True,
                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
subprocess.run(["python3", "wait.py", "service.port"], check=True)
"#,
        )
        .expect("detached service launcher");
        let mut agent = AgentProcess::start(&fixture, &[]);
        agent.wait_for("ready", |record| record["kind"] == "ready");
        agent.send(&prompt_frame("start the fixture HTTP service"));
        wait_for_fixture_file(&fixture.workspace().join("service.port"));
        let port: u16 = std::fs::read_to_string(fixture.workspace().join("service.port"))
            .expect("service port")
            .parse()
            .expect("numeric service port");
        let endpoint = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let service_namespace =
            std::fs::read_to_string(fixture.workspace().join("service.namespace"))
                .expect("service namespace");
        let host_namespace = std::fs::read_link("/proc/self/ns/net")
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let isolated = service_namespace != host_namespace;
        // A port in another namespace cannot identify a host listener. Only
        // the shared-network fixture may receive the cleanup request.
        let cleanup = (!isolated).then_some(ServiceCleanup(endpoint));
        // Full Linux containment has its own loopback. The server's self-
        // request proves startup; an external localhost request cannot do so.
        assert_eq!(service_request(endpoint, "/").is_some(), !isolated);
        std::fs::write(fixture.workspace().join("service.probed"), "observed")
            .expect("release the turn after the startup probe");
        agent.wait_for("assistant_done", |record| {
            record["kind"] == "assistant_done"
        });
        let after_turn = service_request(endpoint, "/");
        assert_eq!(
            after_turn.is_some(),
            !isolated,
            "service reachability after the turn differed for detached={detached}, isolated={isolated}"
        );
        agent.send(&quit_frame());
        let outcome = agent.finish();
        assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
        outcome.assert_exactly_one_terminal_record();
        let after = service_request(endpoint, "/");
        assert_eq!(
            after.is_some(),
            detached && !isolated,
            "service reachability after Plak shutdown differed for detached={detached}, isolated={isolated}"
        );
        drop(cleanup);
    }
}

struct ServiceCleanup(SocketAddr);

impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        let _ = service_request(self.0, "/stop");
    }
}

fn service_request(endpoint: SocketAddr, path: &str) -> Option<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&endpoint, Duration::from_millis(500)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
        .ok()?;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).ok()?;
    answer
        .windows(b"service-ready".len())
        .any(|part| part == b"service-ready")
        .then_some(answer)
}

/// A live process session when the agent is stopped by a signal.
///
/// The teardown runs on the way out rather than on a tool call, so the only
/// thing a face can say about it is that the stream still ends exactly once and
/// the process still exits. Both are what a supervisor sees.
#[test]
fn a_live_process_session_does_not_stop_a_signalled_agent_from_terminating() {
    let fixture = Fixture::new(&process_session_script("sleep 30", false, &json!([])));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("start a session and leave it running"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    signal(agent.id(), SIGTERM);
    agent.close_stdin();
    let outcome = agent.finish();
    outcome.assert_exactly_one_terminal_record();
    assert!(
        matches!(outcome.status.code(), Some(0 | SIGNALLED_EXIT_CODE)),
        "code was {:?}, stderr:\n{}",
        outcome.status.code(),
        outcome.stderr
    );
}

/// A turn cut while a tool was still running is abandoned, and the note says
/// the command may have run.
///
/// This is the common crash for a coding agent: the model asks for a command,
/// the command takes a while, and the process dies inside it. Abandonment used
/// to cover only a turn cut inside its model request, which left this shape
/// unadoptable and the session lost for good. Live, that was the difference
/// between two accounts on the same case — the slower model was still inside
/// its first request and recovered, the faster one had already called a tool
/// and was refused on every attempt.
///
/// What abandonment may not do is pretend the tool did nothing. The command may
/// have run to completion and changed the workspace, and nothing afterwards can
/// find out, so the note says exactly that and tells the user to look.
#[test]
fn a_turn_cut_while_a_tool_was_running_is_abandoned_and_says_the_command_may_have_run() {
    let fixture = Fixture::new(&sleeping_tool_script("never reached"));
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
    let session = hello["session_id"].as_str().expect("session id").to_owned();
    agent.send(&prompt_frame("start a long command"));
    agent.wait_for("the tool to start running", |record| {
        record["kind"] == "tool" && record["name"] == "process_session_start"
    });
    let orphan_deadline = Instant::now() + Duration::from_secs(5);
    while !orphan_is_running() && Instant::now() < orphan_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    signal(agent.id(), SIGKILL);
    agent.close_stdin();
    let _ = agent.finish();
    // The child outlives the agent that started it. That is the case worth
    // pinning: the reopen happens while the command is still running, so the
    // product cannot claim it finished and cannot claim it did not.
    assert!(
        orphan_is_running(),
        "the child did not outlive the killed agent, so this is not the orphan case"
    );

    fixture.write_script(&process_tool_answer_script("recovered and answered"));
    let deadline = Instant::now() + WAIT;
    let recovered = loop {
        let run = run_cli(&fixture, &["run", "--session", &session, "--", "carry on"]);
        if run.status.success() {
            break run;
        }
        assert!(
            Instant::now() < deadline,
            "a turn cut inside a tool never reopened; stderr was:\n{}",
            run.stderr
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(
        recovered.stdout.contains("recovered and answered"),
        "the recovered session did not answer; stdout was: {}",
        recovered.stdout
    );
    assert!(
        recovered.stderr.contains(RECOVERY_NOTE),
        "the recovery note is missing; stderr was:\n{}",
        recovered.stderr
    );
    // A turn cut while a tool was running. It must make the running-tool claim
    // and must not borrow the model-request one: this turn's request was
    // answered, so "may have been billed" would be false here.
    assert_note_claims_only(
        &recovered.stderr,
        &RUNNING_TOOL_ONLY,
        &[MODEL_REQUEST_ONLY[0], NOTHING_STARTED_ONLY[0]],
        "running-tool",
    );
    assert_abandonment_classification(&fixture, "running_tool", "process_lost");
    let kinds = session_log_kinds(&fixture);
    assert!(
        kinds.iter().any(|kind| kind == "turn.abandoned"),
        "the session log has no turn.abandoned record; kinds were {kinds:?}"
    );
    reap_orphan();
}

/// The command this fixture's child runs, spelled so it can be found by name.
///
/// A duration nothing else in this file uses, so `pgrep` finds this test's
/// orphan and not a neighbour's child running in parallel.
const ORPHAN_COMMAND: &str = "sleep 33";

/// Whether this fixture's orphaned child is still running.
fn orphan_is_running() -> bool {
    Command::new("pgrep")
        .arg("-f")
        .arg(ORPHAN_COMMAND)
        .output()
        .is_ok_and(|out| !out.stdout.is_empty())
}

/// Kills whatever this fixture's child left behind, so the suite does not leak.
fn reap_orphan() {
    let _ = Command::new("pkill").arg("-f").arg(ORPHAN_COMMAND).status();
}

/// One scripted turn that starts a real child and blocks inside the tool.
///
/// The foreground observation hint makes the cut land inside the effect rather
/// than after it: the tool does not return until the command does, so a signal
/// during the sleep leaves the effect outstanding.
fn sleeping_tool_script(answer: &str) -> Value {
    json!({
        "version": 1,
        "process_tools": true,
        "steps": [
            {"output": {"tool": {"name": "process_session_start", "arguments": {
                "command": ORPHAN_COMMAND,
                "workdir": ".",
                "login": false,
                "tty": false,
                "keep_stdin_open": true,
                "yield_time_ms": 30000,
                "max_output_bytes": 8192
            }}}, "finish": "tool_calls"},
            {"output": {"text": answer}}
        ]
    })
}

/// One immediate answer, on a binding that still carries the process tools.
///
/// A reopened session is compiled against the tools its history names, so a
/// recovery script that dropped them would fail on the binding rather than on
/// anything this test is about.
fn process_tool_answer_script(answer: &str) -> Value {
    json!({
        "version": 1,
        "process_tools": true,
        "steps": [{"output": {"text": answer}}]
    })
}

/// A real OS child the backend owns: it reports its own pid, traps TERM, and
/// leaves a marker only if the graceful half of teardown ever reaches it.
///
/// `stopped.txt` is the proof the process group got SIGTERM: the KILL half of
/// the bound never runs a trap, so the marker cannot appear on the forceful
/// path. `owned.pid` records the pid as the child saw it, which on macOS is
/// the host pid and on Linux is namespace-local — only the marker is portable.
/// The shell waits in a builtin read of its own FIFO, so TERM runs the trap at
/// once instead of after a foreground child; a deferred trap can lose the
/// marker to the bounded stop's KILL under load.
fn owned_shutdown_command() -> &'static str {
    "trap 'printf stopped > stopped.txt; exit 0' TERM\n\
     mkfifo stop-wait\n\
     exec 3<>stop-wait\n\
     printf '%s' \"$$\" > owned.pid\n\
     read line <&3\n"
}

/// [`sleeping_tool_script`] over [`owned_shutdown_command`]: the turn parks
/// inside the foreground observation window until the face stops it.
fn owned_process_script(answer: &str) -> Value {
    let mut script = sleeping_tool_script(answer);
    script["steps"][0]["output"]["tool"]["arguments"]["command"] = json!(owned_shutdown_command());
    script
}

fn promoted_run_script(command: &str) -> Value {
    let mut script = sleeping_tool_script("first turn answer");
    script["steps"][0]["output"]["tool"]["arguments"]["command"] = json!(command);
    script["steps"][0]["output"]["tool"]["arguments"]["yield_time_ms"] = json!(0);
    script["steps"].as_array_mut().unwrap().push(json!({
        "output": {"text": "last turn answer"}
    }));
    script
}

/// One line of [`owned_shutdown_command`], since the run face echoes it.
const RUN_OWNED_COMMAND: &str = "trap 'printf stopped > stopped.txt; exit 0' TERM; mkfifo stop-wait; exec 3<>stop-wait; printf '%s' \"$$\" > owned.pid; read line <&3";

struct RunJobCli {
    child: Child,
    stderr: std::sync::mpsc::Receiver<String>,
    lines: Vec<String>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl RunJobCli {
    fn start(fixture: &Fixture, bounds: &[&str]) -> Self {
        let mut args = vec![
            "run",
            "--provider",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
        ];
        args.extend_from_slice(bounds);
        args.extend_from_slice(&["--", "start a background job"]);
        let mut child = spawn_cli(fixture, &args);
        let pipe = child.stderr.take().unwrap();
        let (sender, stderr) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                if sender.send(line.expect("stderr line")).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stderr,
            lines: Vec::new(),
            reader: Some(reader),
        }
    }

    fn wait_for_line(&mut self, expected: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let line = self
                .stderr
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("missing {expected:?}: {error}; {:?}", self.lines));
            let found = line == expected;
            self.lines.push(line);
            if found {
                return;
            }
        }
    }

    fn finish(self) -> CliRun {
        self.finish_within(WAIT)
    }

    fn finish_within(mut self, bound: Duration) -> CliRun {
        let mut pipe = self.child.stdout.take().unwrap();
        let stdout = std::thread::spawn(move || {
            let mut text = String::new();
            pipe.read_to_string(&mut text).unwrap();
            text
        });
        let deadline = Instant::now() + bound;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "run did not exit: {:?}",
                self.lines
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        self.reader.take().unwrap().join().unwrap();
        self.lines.extend(self.stderr.try_iter());
        CliRun {
            status,
            stdout: stdout.join().unwrap(),
            stderr: self.lines.join("\n"),
        }
    }
}

impl Drop for RunJobCli {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn run_waits_for_a_real_promoted_process_then_continues_and_prints_only_last_answer() {
    let command = "printf ready > ready.txt; while [ ! -f release.txt ]; do sleep 0.1; done";
    let fixture = Fixture::new(&promoted_run_script(command));
    let mut run = RunJobCli::start(&fixture, &[]);
    let waiting = format!("plak: waiting for p1 ({command}); Ctrl-C to stop");
    run.wait_for_line(&waiting);
    assert!(
        run.child.try_wait().unwrap().is_none(),
        "run must block while the job lives"
    );
    wait_for_fixture_file(&fixture.workspace().join("ready.txt"));
    std::fs::write(fixture.workspace().join("release.txt"), "exit").unwrap();
    run.wait_for_line("plak: continuing (turn 2: p1 exited 0)");
    let result = run.finish();
    assert!(result.status.success(), "{}", result.stderr);
    assert_eq!(result.stdout, "last turn answer\n");
    assert_eq!(result.stderr.matches(&waiting).count(), 1);
    assert_eq!(
        result
            .stderr
            .matches("plak: continuing (turn 2: p1 exited 0)")
            .count(),
        1
    );
}

#[test]
fn run_max_turns_stops_a_real_promoted_process_and_exits_six() {
    // The stop lands as soon as the one turn ends, so that turn first joins a
    // second process that exits once `owned.pid` exists: the promoted job
    // writes it only after its TERM trap is installed, and a stop that beat
    // the trap would leave no marker.
    let mut script = promoted_run_script(RUN_OWNED_COMMAND);
    script["steps"].as_array_mut().unwrap().insert(
        1,
        json!({"output": {"tool": {"name": "process_session_start", "arguments": {
            "command": "while [ ! -f owned.pid ]; do sleep 0.05; done",
            "workdir": ".",
            "login": false,
            "tty": false,
            "yield_time_ms": 30000,
            "max_output_bytes": 8192
        }}}, "finish": "tool_calls"}),
    );
    let fixture = Fixture::new(&script);
    let result = RunJobCli::start(&fixture, &["--max-turns", "1"]).finish();
    assert_eq!(result.status.code(), Some(6), "{}", result.stderr);
    assert!(
        result
            .stderr
            .lines()
            .any(|line| line == "plak: run stopped at max-turns 1; stopped p1"),
        "{}",
        result.stderr
    );
    assert_eq!(result.stdout, "first turn answer\n");
    assert_owned_process_stopped(&fixture.workspace());
}

#[test]
fn run_timeout_stops_a_real_promoted_process_while_waiting_and_exits_six() {
    let command = RUN_OWNED_COMMAND;
    let fixture = Fixture::new(&promoted_run_script(command));
    // The run timeout counts from process start, so it can fire before the
    // job installs its TERM trap and leave no marker. Fifteen seconds did on
    // a loaded lane; the bound is startup liveness, as in
    // `prepare_bound_child_job`, and `owned.pid` proves the trap came first.
    let mut run = RunJobCli::start(&fixture, &["--timeout", "60"]);
    run.wait_for_line(&format!("plak: waiting for p1 ({command}); Ctrl-C to stop"));
    wait_for_fixture_file(&fixture.workspace().join("owned.pid"));
    let result = run.finish_within(Duration::from_secs(90));
    assert_eq!(result.status.code(), Some(6), "{}", result.stderr);
    assert!(
        result
            .stderr
            .lines()
            .any(|line| line == "plak: run stopped at timeout; stopped p1"),
        "{}",
        result.stderr
    );
    assert_eq!(result.stdout, "first turn answer\n");
    assert_owned_process_stopped(&fixture.workspace());
}

#[test]
fn run_sigint_while_waiting_stops_a_real_promoted_process_and_exits_four() {
    // The sleep before the trap makes a stop sent before `owned.pid` exists
    // lose the marker every time, not only under load.
    let command = format!("sleep 1; {RUN_OWNED_COMMAND}");
    let fixture = Fixture::new(&promoted_run_script(&command));
    let mut run = RunJobCli::start(&fixture, &[]);
    run.wait_for_line(&format!("plak: waiting for p1 ({command}); Ctrl-C to stop"));
    wait_for_fixture_file(&fixture.workspace().join("owned.pid"));
    signal(run.child.id(), SIGINT);
    let result = run.finish();
    assert_eq!(result.status.code(), Some(4), "{}", result.stderr);
    assert_eq!(result.stdout, "first turn answer\n");
    assert!(!result.stderr.contains("plak: continuing"));
    assert_owned_process_stopped(&fixture.workspace());
}

#[test]
fn run_timeout_records_busy_and_quiescent_child_terminals_for_exact_reopen() {
    run_bound_records_child_jobs(false, &[false, true]);
}

#[test]
fn js_ptc_real_nested_exec_promotes_and_reports_nested_telemetry() {
    let fixture = Fixture::new(&json!({
        "version":1,"code_mode":true,"js_ptc":true,
        "steps":[
            {"output":{"tool":{"name":"exec_js","arguments":{
                "source":"const job = await tools.exec_command({cmd: 'sleep 60', yield_time_ms: 0}); text(job);",
                "yield_time_ms":10_000
            }}},"finish":"tool_calls"},
            {"output":{"text":"nested process promoted"}}
        ]
    }));
    let result = run_cli(
        &fixture,
        &[
            "run",
            "--provider",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--max-turns",
            "1",
            "--",
            "start a nested job",
        ],
    );
    assert_eq!(result.status.code(), Some(6), "{}", result.stderr);
    assert_eq!(result.stdout, "nested process promoted\n");
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let connection = rusqlite::Connection::open(paths.telemetry_database()).unwrap();
    let nested: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM job_events WHERE kind='promoted' AND nested=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(nested, 1, "real nested process promotion was not observed");
}

#[test]
fn run_sigint_records_child_owned_process_terminal_for_exact_reopen() {
    run_bound_records_child_jobs(true, &[false]);
}

fn run_bound_records_child_jobs(sigint: bool, modes: &[bool]) {
    // Both real processes spend the same timeout window alive; every fixture
    // still proves its own terminal, shutdown marker and exact durable reopen.
    let runs: Vec<_> = modes
        .iter()
        .map(|quiescent| prepare_bound_child_job(sigint, *quiescent))
        .collect();
    for (fixture, run) in runs {
        let result = if sigint {
            signal(run.child.id(), SIGINT);
            run.finish()
        } else {
            run.finish_within(Duration::from_secs(90))
        };
        assert_bound_child_job_exact_reopen(&fixture, &result, sigint);
    }
}

fn prepare_bound_child_job(sigint: bool, quiescent: bool) -> (Fixture, RunJobCli) {
    let fixture = Fixture::new(&json!({
        "version":1,"tree":true,"durable_tree":true,"child_process":true,
        "child_process_quiescent":quiescent,
        "steps":[
            {"output":{"tool":{"name":"spawn_agent","arguments":{
                "template":"implement","tier":"mutate","workspace":"fork",
                "description":"child-owned job","prompt":"hold a process",
                "output_schema_json":"{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
            }}},"finish":"tool_calls"},
            {"output":{"text":"root is waiting"}}
        ]
    }));
    // The run timeout is counted from process start. Fifteen seconds has
    // fired during the child's `sleep 1` (before its TERM trap) on a cold
    // loaded lane, so `stopped.txt` never appears. The bound is liveness
    // for startup, not the behavior under test: the timeout must still
    // land only after `owned.pid` proves the trap is installed. `finish`
    // then has to outlast whatever of that bound remains.
    let bounds: &[&str] = if sigint {
        &["--print-session-id"]
    } else {
        &["--print-session-id", "--timeout", "60"]
    };
    let mut run = RunJobCli::start(&fixture, bounds);
    run.wait_for_line("plak: waiting for c1; Ctrl-C to stop");
    // The child's shell sleeps before it traps TERM; a stop sent before
    // `owned.pid` exists would kill it without the marker.
    wait_for_owned_pid_under(&fixture.root);
    (fixture, run)
}

fn assert_bound_child_job_exact_reopen(fixture: &Fixture, result: &CliRun, sigint: bool) {
    use plakous::sessions::{
        AgentSessions, OpenSessionOutcome, OpenSessionRequest, SessionBackend, SessionLocator,
        SessionOperationId,
    };
    assert_eq!(
        result.status.code(),
        Some(if sigint { 4 } else { 6 }),
        "{}",
        result.stderr
    );
    let root_id = result.session_id();
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let tree = plak::store::tree::load_tree(&paths, &root_id)
        .unwrap()
        .unwrap();
    let child = tree.node("c1").unwrap();
    assert_eq!(
        child.status,
        plakous_hosted::agent_tree_protocol::NodeStatus::Interrupted
    );
    assert_owned_process_stopped(&child.path);
    let locator: SessionLocator = child.session_locator.as_ref().unwrap().parse().unwrap();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        child.path.join("reopen-evidence.jsonl"),
    )
    .unwrap();
    let plan =
        Arc::new(serde_json::from_value(process_tool_answer_script("reopened child")).unwrap());
    let binding =
        plakous_offline_crash_harness::scripted_runtime_binding(&child.path, controller, plan)
            .unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    executor
        .block_on(async {
            let OpenSessionOutcome::Opened { session, .. } = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new("child-job:exact-reopen").unwrap(),
                    locator,
                    binding,
                ))
                .await
                .unwrap()
            else {
                panic!("exact child reopen required");
            };
            let jobs = session.agent().jobs();
            assert_eq!(jobs.len(), 1);
            assert_eq!(
                jobs[0].status,
                plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::SessionExit)
            );
            session.release().await.unwrap();
        })
        .unwrap();
}

/// Bounded wait for a fixture file the owned process itself writes.
fn wait_for_fixture_file(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Bounded wait for an `owned.pid` anywhere a few levels under `root`. The
/// owned process writes it only after installing its TERM trap.
fn wait_for_owned_pid_under(root: &Path) {
    fn find(dir: &Path, depth: usize) -> bool {
        dir.join("owned.pid").is_file()
            || depth > 0
                && std::fs::read_dir(dir).is_ok_and(|entries| {
                    entries
                        .flatten()
                        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                        .any(|entry| find(&entry.path(), depth - 1))
                })
    }
    let deadline = Instant::now() + WAIT;
    while !find(root, 5) {
        assert!(
            Instant::now() < deadline,
            "no owned.pid appeared under {}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `/bin/kill -0`, quiet: a reaped pid is the expected answer, not an error.
#[cfg(target_os = "macos")]
fn host_process_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Asserts the owned child saw the graceful teardown: the TERM trap wrote
/// `stopped.txt`, and on macOS the reported pid is gone. Linux writes a
/// namespace pid no host `kill -0` could identify, so the marker plus the
/// backend's own bounded group cleanup carry the claim there.
fn assert_owned_process_stopped(workspace: &Path) {
    wait_for_fixture_file(&workspace.join("stopped.txt"));
    #[cfg(target_os = "macos")]
    {
        let pid = workspace
            .join("owned.pid")
            .exists()
            .then(|| {
                std::fs::read_to_string(workspace.join("owned.pid"))
                    .ok()
                    .and_then(|body| body.trim().parse::<u32>().ok())
            })
            .flatten()
            .expect("owned.pid carried a pid");
        let deadline = Instant::now() + WAIT;
        while host_process_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "owned process {pid} outlived the product that owned it"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Signals the proven owned pid if a failing test would otherwise leak the
/// sleep loop. Never armed before `owned.pid` exists, and never signalled on
/// Linux: the written `$$` is namespace-local there and could name an
/// unrelated host process, which is exactly the wrong thing to kill.
#[derive(Default)]
struct OwnedProcessGuard(Option<u32>);

impl OwnedProcessGuard {
    fn arm(&mut self, workspace: &Path) {
        self.0 = std::fs::read_to_string(workspace.join("owned.pid"))
            .ok()
            .and_then(|body| body.trim().parse::<u32>().ok());
    }
}

#[cfg(target_os = "macos")]
impl Drop for OwnedProcessGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0
            && host_process_alive(pid)
        {
            let _ = Command::new("/bin/kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status();
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Drop for OwnedProcessGuard {
    fn drop(&mut self) {}
}

/// The session id the cooked-terminal resume hint printed.
fn resume_hint_session(raw: &[u8]) -> Option<String> {
    let cooked = String::from_utf8_lossy(raw);
    cooked
        .split("plak resume ")
        .nth(1)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

/// The `*.events.jsonl` logs this fixture home holds, one per session, sorted.
fn session_event_files(fixture: &Fixture) -> Vec<PathBuf> {
    let mut files = std::fs::read_dir(
        plak::home::paths::PlakPaths::explicit(fixture.home().clone()).sessions_dir(),
    )
    .map(|entries| {
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".events.jsonl"))
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    files.sort();
    files
}

/// §6 quit over a live OS child on the real TUI: the terminal is restored,
/// the worker join holds until the child's group TERM lands, and the printed
/// resume id attaches the same session for a second turn — not a fresh one.
#[test]
fn real_process_quit_releases_and_resumes_exact_session() {
    real_process_quit_fixture(false);
}

#[test]
fn real_promoted_job_quit_releases_and_resumes_exact_session() {
    real_process_quit_fixture(true);
}

#[test]
fn processes_pty_idle_exit_refreshes_live_count_without_another_turn() {
    let plan = promoted_run_script(
        "printf '%s' \"$$\" > owned.pid; while [ ! -f finish-job ]; do sleep 0.01; done",
    );
    let fixture = Fixture::new(&plan);
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.home().join("config.toml"))
        .expect("config");
    writeln!(config, "\n[turn]\nauto_continue = false").expect("disable auto-continue");
    let mut guard = OwnedProcessGuard::default();
    let mut pilot = PilotClient::new(fixture.root.join("pilot"));
    let mut terminal = TerminalChild::spawn_with_pilot(&fixture, 100, None, Some(&pilot.root));
    terminal.wait_screen("/10M");
    terminal.send(b"start a background job\r");
    wait_for_fixture_file(&fixture.workspace().join("owned.pid"));
    guard.arm(&fixture.workspace());
    terminal.wait_screen("first turn answer");
    pilot.observe_until("running idle process", |document| {
        document["snapshot"]["background_processes"] == 1
    });
    std::fs::write(fixture.workspace().join("finish-job"), "finish").expect("release job");
    pilot.observe_until("idle process exit", |document| {
        document["snapshot"]["background_processes"] == 0
    });
    terminal.send(b"\x11");
    assert!(terminal.finish().success());
    guard.0 = None;
}

#[test]
fn processes_pty_journey_opens_ps_and_stops_background_job() {
    let mut plan = owned_process_script("background process ready");
    plan["steps"][0]["output"]["tool"]["arguments"]["yield_time_ms"] = json!(0);
    // Stop closes stdin before TERM. Hold a separate FIFO in a builtin read,
    // avoiding both early EOF and trap deferral behind a foreground sleep.
    plan["steps"][0]["output"]["tool"]["arguments"]["command"] = json!(
        "trap 'printf stopped > stopped.txt; exit 0' TERM\n\
         mkfifo stop-wait\n\
         exec 3<>stop-wait\n\
         printf '%s' \"$$\" > owned.pid\n\
         read line <&3\n"
    );
    let fixture = Fixture::new(&plan);
    let mut guard = OwnedProcessGuard::default();
    let mut pilot = PilotClient::new(fixture.root.join("pilot"));
    let mut terminal = TerminalChild::spawn_with_pilot(&fixture, 100, None, Some(&pilot.root));
    terminal.wait_screen("/10M");
    terminal.send(b"start a background job\r");
    wait_for_fixture_file(&fixture.workspace().join("owned.pid"));
    guard.arm(&fixture.workspace());
    terminal.wait_screen("background process ready");
    pilot.observe_until("running process count", |document| {
        document["snapshot"]["background_processes"] == 1
    });
    terminal.send(b"/ps\r");
    terminal.wait_screen("Processes");
    pilot.observe_until("Processes overlay", |document| {
        document["snapshot"]["owner"] == "processes"
    });
    assert_eq!(
        pilot.act("process-stop", None)["snapshot"]["owner"],
        "destructive-confirmation"
    );
    pilot.act("confirm", None);
    pilot.observe_until("stopped background process", |document| {
        document["snapshot"]["background_processes"] == 0
    });
    assert_owned_process_stopped(&fixture.workspace());
    terminal.send(b"\x11");
    assert!(terminal.finish().success());
    guard.0 = None;
}

fn real_process_quit_fixture(promoted: bool) {
    let mut plan = owned_process_script("job is running in background");
    if promoted {
        plan["steps"][0]["output"]["tool"]["arguments"]["yield_time_ms"] = json!(0);
    }
    let fixture = Fixture::new(&plan);
    let mut guard = OwnedProcessGuard::default();
    let mut terminal = TerminalChild::spawn(&fixture, 100, None);
    terminal.wait_screen("/10M");
    terminal.send(b"run the owned process\r");
    wait_for_fixture_file(&fixture.workspace().join("owned.pid"));
    guard.arm(&fixture.workspace());

    if promoted {
        terminal.wait_screen("job is running in background");
    }
    // The budget is for quitting, so the command row is painted first: the
    // first bash highlight in a process loads and compiles the syntax set,
    // which on a loaded debug build alone can exceed a second.
    terminal.wait_screen("trap 'printf stopped");

    // Quit asks first, mid-turn or over the promoted job.
    terminal.send(b"\x11");
    terminal.wait_screen("Quit plak?");
    if promoted {
        terminal.wait_screen("background process will be stopped");
    }
    // Focus starts on Quit, so Enter confirms. The budget runs from that key.
    let quit_started = Instant::now();
    terminal.send(b"\r");
    let (status, raw) = terminal.finish_with_raw();
    assert!(
        quit_started.elapsed() < Duration::from_secs(1),
        "quit exceeded its hard budget: {:?}",
        quit_started.elapsed()
    );
    assert!(
        status.success(),
        "the TUI quit did not exit cleanly; cooked output was:\n{}",
        String::from_utf8_lossy(&raw)
    );
    let leave = raw
        .windows(b"\x1b[?1049l".len())
        .position(|window| window == b"\x1b[?1049l")
        .expect("the quit must leave the alternate screen");
    assert!(
        raw_contains(&raw[leave..], b"\x1b[?25h"),
        "the cursor must be shown after the alternate screen is left"
    );
    assert_owned_process_stopped(&fixture.workspace());
    let session = resume_hint_session(&raw)
        .expect("a saved session must print its resume hint to the cooked terminal");
    let before = session_event_files(&fixture);
    assert_eq!(
        before.len(),
        1,
        "one saved session expected, got {before:?}"
    );
    assert!(
        before[0]
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&session)),
        "the resume hint {session} must prefix the saved session record {before:?}"
    );

    fixture.write_script(&process_tool_answer_script("resumed owned process session"));
    let mut resumed = TerminalChild::spawn(&fixture, 100, Some(&session));
    resumed.wait_screen("/ commands");
    resumed.send(b"carry on\r");
    resumed.wait_screen("resumed owned process session");
    resumed.send(b"\x11");
    assert!(resumed.finish().success());

    assert_eq!(
        session_event_files(&fixture),
        before,
        "the resume must attach the same session, not open a fresh one"
    );
}

/// The same lifecycle on the JSONL face: interrupt then quit is the product's
/// own shutdown order, and the typed lifecycle correlates one turn id from
/// admitted to its real terminal — never a fabricated `completed`.
#[test]
fn real_process_jsonl_quit_releases_and_resumes_exact_session() {
    let fixture = Fixture::new(&owned_process_script("unreachable"));
    let mut guard = OwnedProcessGuard::default();
    let mut agent = AgentProcess::start(&fixture, &[]);
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
    let session = hello["session_id"].as_str().expect("session id").to_owned();
    agent.send(&prompt_frame("run the owned process"));
    wait_for_fixture_file(&fixture.workspace().join("owned.pid"));
    guard.arm(&fixture.workspace());

    agent.send(&json!({"v": 1, "kind": "interrupt"}));
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_owned_process_stopped(&fixture.workspace());

    let lifecycles = outcome
        .records()
        .into_iter()
        .filter(|record| record["kind"] == "turn_lifecycle")
        .collect::<Vec<_>>();
    let admitted = lifecycles
        .iter()
        .find(|record| record["phase"] == "admitted")
        .expect("the turn admitted a lifecycle marker");
    let turn_id = admitted["turn_id"].as_str().expect("turn id").to_owned();
    let terminals = lifecycles
        .iter()
        .filter(|record| record["turn_id"] == turn_id && record["phase"] != "admitted")
        .filter_map(|record| record["phase"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        terminals,
        ["cancelled"],
        "the interrupted turn's terminal evidence was {lifecycles:?}"
    );
    assert!(
        !lifecycles
            .iter()
            .any(|record| record["phase"] == "completed"),
        "a command that never returned must not report completed: {lifecycles:?}"
    );

    fixture.write_script(&process_tool_answer_script("resumed owned process session"));
    let mut resumed = AgentProcess::start(&fixture, &["--session", &session]);
    resumed.wait_for("ready", |record| record["kind"] == "ready");
    resumed.send(&prompt_frame("carry on"));
    resumed.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    resumed.send(&quit_frame());
    let second = resumed.finish();
    assert!(second.status.success(), "stderr:\n{}", second.stderr);
    assert_eq!(second.session_id(), session);
    assert!(
        second
            .assistant_text()
            .iter()
            .any(|text| text == "resumed owned process session"),
        "stream:\n{}",
        second.lines.join("\n")
    );
}

/// The CLI counterpart: SIGINT at a live owned child exits with the signal
/// code, the child sees TERM, and the same session resumes for another turn.
#[test]
fn real_process_cli_sigint_releases_and_resumes_exact_session() {
    let fixture = Fixture::new(&owned_process_script("unreachable"));
    let mut guard = OwnedProcessGuard::default();
    let mut child = spawn_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "run the owned process",
        ],
    );
    wait_for_fixture_file(&fixture.workspace().join("owned.pid"));
    guard.arm(&fixture.workspace());
    signal(child.id(), SIGINT);
    let output = wait_with_output(&mut child);
    assert_eq!(
        output.status.code(),
        Some(SIGNALLED_EXIT_CODE),
        "stderr:\n{}",
        output.stderr
    );
    let session = output.session_id();
    assert_owned_process_stopped(&fixture.workspace());

    fixture.write_script(&process_tool_answer_script("resumed owned process session"));
    let resumed = run_cli(&fixture, &["run", "--session", &session, "--", "carry on"]);
    assert!(
        resumed.status.success(),
        "resume failed: {}\n{}",
        resumed.stdout,
        resumed.stderr
    );
    assert!(
        resumed.stdout.contains("resumed owned process session"),
        "resumed stdout was: {}",
        resumed.stdout
    );
}

/// The call id of the one completed outer call named `name`.
///
/// Panics rather than returning an option: every caller is asserting that the
/// call happened, and a missing id is that assertion failing, not a branch.
fn outer_call_id(outcome: &AgentOutcome, name: &str) -> String {
    let ids = outcome
        .records()
        .iter()
        .filter(|record| record["kind"] == "tool" && record["name"] == name)
        .filter_map(|record| record["call_id"].as_str())
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        ids.len(),
        1,
        "expected exactly one {name} call, saw {ids:?}; stream:\n{}",
        outcome.lines.join("\n")
    );
    ids.into_iter().next().unwrap_or_default()
}

/// The names of the calls a composed call fathered, in stream order.
///
/// **Parentage, not name.** A cell reaches a nested tool by the name the
/// evaluator exposes, `exec_command`, but the product publishes the activity
/// under the registered tool it lowers onto, which is `process_session_start`.
/// Asserting on the exposed name tests the evaluator's vocabulary; asserting on
/// `parent_call_id` tests what the product actually promises, which is that a
/// nested call is attributed to the cell that made it.
fn nested_call_names(outcome: &AgentOutcome, parent: &str) -> Vec<String> {
    outcome
        .records()
        .iter()
        .filter(|record| record["kind"] == "tool" && record["state"] == "done")
        .filter(|record| record["parent_call_id"] == parent)
        .filter_map(|record| record["name"].as_str())
        .map(str::to_owned)
        .collect()
}

/// The tools that reported `done`, in order.
fn completed_tool_names(outcome: &AgentOutcome) -> Vec<String> {
    outcome
        .records()
        .iter()
        .filter(|record| record["kind"] == "tool" && record["state"] == "done")
        .filter_map(|record| record["name"].as_str())
        .map(str::to_owned)
        .collect()
}

/// The stream so far, for an assertion made while the process is still live.
fn agent_snapshot(agent: &AgentProcess) -> AgentOutcome {
    AgentOutcome {
        status: std::process::ExitStatus::default(),
        lines: agent.snapshot(),
        stderr: String::new(),
    }
}

// ---------------------------------------------------------------------------
// Observation clipping
// ---------------------------------------------------------------------------

/// A tool result far larger than the remaining observation budget.
///
/// The cap is the honest dial. The derived budget is
/// `(headroom_tokens - 4096) * 4` bytes, which on any realistic route is
/// hundreds of kilobytes, so squeezing it through `maximum_projected_input`
/// means picking a ceiling barely above the 4,096-token reserve and getting a
/// budget of *zero*, which clips everything and tests a degenerate case.
/// `observation_budget_cap_bytes` only ever shrinks the derived budget, so
/// 2,048 against a 60,000-byte body keeps a real prefix and drops a real
/// remainder, which is what an operator actually meets.
fn clipped_observation_script() -> Value {
    json!({
        "version": 1,
        "observation_budget_cap_bytes": 2_048,
        "tool_result_bytes": 60_000,
        "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": "answered from the kept portion"}}
        ]
    })
}

/// The figures an `observation clipped` note has to carry.
///
/// A bare "truncated" tells a reader nothing about whether the model saw
/// enough, so the note names the tool, what stayed, what went, and the bound
/// the cut was measured against.
fn assert_clip_note_is_complete(note: &str) {
    assert!(
        note.contains("observation clipped: loopback_effect returned more than the remaining observation budget;"),
        "the clip note did not name its tool: {note}"
    );
    // The exact arithmetic, not just the shape. `bytes_kept` is budget
    // consumed rather than the length of the clipped string, which carries a
    // truncation notice of its own, so kept plus dropped is the original body
    // and neither equals what appears in the transcript.
    assert!(
        note.contains("2048 bytes kept, 57952 dropped, against a 2048-byte bound"),
        "the clip note did not account for the whole body: {note}"
    );
    assert!(
        note.contains(
            "The model is answering from the kept portion; narrow the call to see the rest."
        ),
        "the clip note did not say what to do about it: {note}"
    );
}

/// The durable half: the clip is a session-log record, not only a note.
fn assert_clip_is_recorded(fixture: &Fixture) {
    let kinds = session_log_kinds(fixture);
    assert!(
        kinds.iter().any(|kind| kind == "observation.clipped"),
        "the session log has no observation.clipped record; kinds were {kinds:?}"
    );
}

/// The clip is a call-scoped event, and it took ten call sites to find that out.
///
/// `clip_tool_result` is reached from ten places. Nine are the read-only and
/// rejection paths; the tenth is `Driver::execute_mutating_effect`, which is
/// where every tool that writes goes, and it used the plain
/// `canonicalize_tool_execution` that discards the clip. An in-process
/// read-only test therefore passed while nothing a real turn does announced
/// anything. These two are the cases that could see that, because they drive a
/// mutating tool through the shipped binary.
///
/// Two traps from finding it, recorded so nobody pays for them again. The
/// `tool.completed` record's `detail` in the session log is truncated at a
/// fixed length regardless of the budget, so its size is not a measurement of
/// anything. And a dropped event is indistinguishable from an unpublished one
/// at a face, which is why no ceiling and no budget cap tried from outside
/// could tell them apart.
#[test]
fn a_clipped_tool_observation_is_announced_on_the_jsonl_face() {
    let fixture = Fixture::new(&clipped_observation_script());
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("read something far too large"));
    let note = agent.wait_for("the observation clip note", |record| {
        record["kind"] == "status"
            && record["detail"]
                .as_str()
                .is_some_and(|detail| detail.starts_with("observation clipped:"))
    });
    assert_clip_note_is_complete(note["detail"].as_str().expect("a note detail"));
    assert_eq!(note["phase"], "note");
    assert_clip_is_recorded(&fixture);
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    // The turn still answers: a clipped read is a truncated observation, not a
    // failure, and the model answers from what it kept.
    assert_eq!(
        outcome.assistant_text(),
        vec!["answered from the kept portion".to_owned()]
    );
}

/// The same clip, as the one line the CLI face carries through.
///
/// See the finding on the JSONL case above.
#[test]
fn a_clipped_tool_observation_is_announced_on_the_cli_face() {
    let fixture = Fixture::new(&clipped_observation_script());
    let run = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--",
            "read something far too large",
        ],
    );
    assert!(run.status.success(), "{}\n{}", run.stdout, run.stderr);
    let Some(note) = run
        .stderr
        .lines()
        .find_map(|line| line.strip_prefix("plak: observation clipped:"))
        .map(|rest| format!("observation clipped:{rest}"))
    else {
        panic!("no clip note on stderr:\n{}", run.stderr);
    };
    assert_clip_note_is_complete(&note);
    assert_eq!(
        run.stderr
            .lines()
            .filter(|line| line.contains("observation clipped:"))
            .count(),
        1,
        "the clip is one event and must be said once:\n{}",
        run.stderr
    );
    assert!(
        run.stdout.contains("answered from the kept portion"),
        "stdout was: {}",
        run.stdout
    );
    assert_clip_is_recorded(&fixture);
}

// ---------------------------------------------------------------------------
// Code Mode
// ---------------------------------------------------------------------------

/// A scripted Code Mode turn.
///
/// `code_mode` attaches a real composition profile, so the model is offered the
/// `exec` and `wait` markers over the Codex minimal nested catalog rather than
/// the bare tools. A `cell` step's program is the `exec` call's arguments
/// verbatim, because that marker's contract is UTF-8 text rather than JSON.
fn code_mode_script(steps: &Value) -> Value {
    let mut plan = json!({"version": 1, "code_mode": true});
    if let (Some(object), Some(steps)) = (plan.as_object_mut(), steps.as_array()) {
        object.insert("steps".to_owned(), Value::Array(steps.clone()));
    }
    plan
}

/// A Code Mode turn whose cell leaves the owned process running through the
/// real nested `exec_command`.
fn code_mode_owned_script() -> Value {
    let command = serde_json::to_string(owned_shutdown_command()).expect("command json");
    code_mode_script(&json!([
        {"output": {"cell": format!(
            "await tools.exec_command({{cmd: {command}, yield_time_ms: 10000}}); text('joined');"
        )}, "finish": "tool_calls"},
        {"output": {"text": "the composed answer"}}
    ]))
}

/// A cell that calls `exec_command` twice and emits both results.
///
/// The program runs as an async module: no top-level return, and visible output
/// goes through `text(..)`. Nested tools are reached only through the global
/// `tools` object and return process result objects.
const TWO_NESTED_CALLS: &str = r#"
const one = await tools.exec_command({ cmd: "printf nested-one" });
const two = await tools.exec_command({ cmd: "printf nested-two" });
if (one.output !== "nested-one" || one.exit_code !== 0 || one.session_id !== undefined) throw new Error("bad first process object");
if (two.output !== "nested-two" || two.exit_code !== 0) throw new Error("bad second process object");
text(one.output + "|" + two.output);
"#;

/// A Code Mode turn whose cell makes two nested calls, on the JSONL face.
///
/// Four things had to be true at once for a scripted model to reach local Code
/// Mode from a spawned process, and each was a separate refusal:
///
/// 1. `plakous-compose` carries one extra enrolled target under
///    `scripted-code-mode-enrolment`, every field a `scripted`/`offline`
///    identifier, so the scripted model's projection target is recognised
///    without widening anything for a real provider.
/// 2. The provider configuration owns **exactly** the `exec` and `wait`
///    markers, in the `LocalCompositionOnly` presentation. The nested catalog
///    lives in the registry, not in the configuration.
/// 3. The harness plan comes from `compile_local_composition_plan`, so the
///    provider-visible exposure is empty. A composition deployment exposes its
///    catalog through the evaluator; the default plan exposes the whole
///    registry and the agent then rejects the pair.
/// 4. The descriptor's context contract names the descriptor's **own** protocol
///    family. Borrowing the loopback contract is refused at admission, not at
///    construction.
///
/// The exposure in (2) and (3) reads like a contradiction and is not: the
/// provider sees two markers, the evaluator sees the catalog.
#[test]
fn a_code_mode_cell_runs_two_nested_calls_on_the_jsonl_face() {
    let fixture = Fixture::new(&code_mode_script(&json!([
        {"output": {"cell": TWO_NESTED_CALLS}, "finish": "tool_calls"},
        {"output": {"text": "the composed answer"}}
    ])));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("run a cell"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(
        outcome.assistant_text(),
        vec!["the composed answer".to_owned()]
    );
    // The cell is one outer call; the two nested calls are its own.
    let outer = completed_tool_names(&outcome);
    assert!(
        outer.iter().any(|name| name == "exec"),
        "the cell never ran; tools were {outer:?}; stream:\n{}",
        outcome.lines.join("\n")
    );
    let nested = nested_call_names(&outcome, &outer_call_id(&outcome, "exec"));
    assert_eq!(
        nested.len(),
        2,
        "expected two nested calls, saw {nested:?}; stream:\n{}",
        outcome.lines.join("\n")
    );
}

/// Exercise both process return paths, polls and catchable tool errors in one
/// real sidecar cell, without timing-dependent process completion.
#[test]
fn code_mode_process_objects_support_nested_polls_and_errors() {
    let source = r#"
const running = await tools.exec_command({cmd: "read line; printf '%s' \"$line\"; exit 7", yield_time_ms: 0});
if (typeof running.session_id !== "number" || running.exit_code !== undefined || typeof running.output !== "string") throw new Error("bad running object");
const polled = await tools.exec_command({session_id: running.session_id, yield_time_ms: 0});
if (polled.session_id !== running.session_id || typeof polled.output !== "string") throw new Error("bad poll object");
const joined = await tools.write_stdin({session_id: running.session_id, chars: "observed\n", yield_time_ms: 1000});
if (joined.exit_code !== 7 || joined.output !== "observed" || joined.session_id !== undefined) throw new Error("bad joined object");
let rejected = false;
try { await tools.write_stdin({session_id: 999999, chars: ""}); }
catch (error) { if (!String(error).length) throw error; rejected = true; }
if (!rejected) throw new Error("missing process rejection");
text("process-object-contract-ok");
"#;
    let fixture = Fixture::new(&code_mode_script(&json!([
        {"output": {"cell": source}, "finish": "tool_calls"},
        {"output": {"text": "checked process objects"}}
    ])));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("check process results"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    let records = outcome.records();
    let result = records
        .iter()
        .find(|record| {
            record["kind"] == "tool" && record["name"] == "exec" && record["state"] == "done"
        })
        .expect("completed cell");
    assert!(
        result.to_string().contains("process-object-contract-ok"),
        "{}",
        outcome.lines.join("\n")
    );
}

/// The same turn on the CLI face.
#[test]
fn a_code_mode_cell_runs_two_nested_calls_on_the_cli_face() {
    let fixture = Fixture::new(&code_mode_script(&json!([
        {"output": {"cell": TWO_NESTED_CALLS}, "finish": "tool_calls"},
        {"output": {"text": "the composed answer"}}
    ])));
    let run = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--",
            "run a cell",
        ],
    );
    assert!(
        run.status.success(),
        "run failed: {}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("the composed answer"),
        "stdout was: {}",
        run.stdout
    );
    // Weaker than the terminal case on purpose, for now: "exec" also occurs in
    // the cell's own program text, so this matches whether or not the cell ran.
    // The composed answer above is what carries the real claim. Tighten this to
    // the CLI face's own composition rendering once that rendering has been
    // observed rather than guessed.
    assert!(
        run.stderr.contains("exec"),
        "the CLI face never rendered the cell; stderr:\n{}",
        run.stderr
    );
}

/// The repository-only MCP fixture server. Workspace test runs build every
/// workspace binary into the directory above the test binary's `deps`.
fn fake_mcp_server() -> PathBuf {
    let executable = std::env::current_exe().expect("test binary");
    let server = executable
        .parent()
        .and_then(Path::parent)
        .expect("target directory")
        .join("mcp-fake-server");
    assert!(
        server.is_file(),
        "{} is missing; run the workspace tests so the fixture binary is built",
        server.display()
    );
    server
}

/// Points the fixture's `[root] agent` template at `mcp:fake`, served by the
/// fixture server with `args`.
pub(super) fn enable_fake_mcp(fixture: &Fixture, args: &[&str]) {
    let config = fixture.home().join("config.toml");
    let mut body = std::fs::read_to_string(&config).expect("config");
    write!(
        body,
        "\n[mcp_servers.fake]\ntransport = \"stdio\"\ncommand = {:?}\nargs = {:?}\n",
        fake_mcp_server().to_str().expect("fixture path"),
        args
    )
    .expect("config body");
    write_private(&config, &body);
    let lead = fixture.home().join("agents").join("lead.toml");
    let mut body = std::fs::read_to_string(&lead).expect("lead template");
    body.push_str("tools = [\"mcp:fake\"]\n");
    write_private(&lead, &body);
}

/// A Code Mode cell reaches a template's MCP tool as a nested call: the
/// product catalog admits it, and the cell gets the server's answer.
#[test]
fn a_code_mode_cell_calls_a_template_mcp_tool_as_a_nested_call() {
    let fixture = Fixture::new(&code_mode_script(&json!([
        {"output": {"cell": "const echoed = await tools.mcp__fake__echo({ text: \"from-mcp\" }); text(echoed);"}, "finish": "tool_calls"},
        {"output": {"text": "the composed answer"}}
    ])));
    let log = fixture.root.join("mcp-calls.log");
    enable_fake_mcp(&fixture, &["--call-log", log.to_str().expect("log path")]);
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("run a cell"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    let nested = nested_call_names(&outcome, &outer_call_id(&outcome, "exec"));
    assert_eq!(
        nested,
        ["mcp__fake__echo"],
        "stream:\n{}",
        outcome.lines.join("\n")
    );
    assert_eq!(std::fs::read_to_string(&log).expect("call log"), "echo\n");
}

#[test]
fn every_session_command_names_a_foreign_build_instead_of_not_found() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let id = "00000000-0000-4000-8000-000000000042";
    let foreign = fixture.home().join("state/r1.e1.p1.t1/sessions");
    std::fs::create_dir_all(&foreign).unwrap();
    write_private(&foreign.join(format!("{id}.events.jsonl")), "");
    for args in [
        vec!["run", "--session", id, "--", "continue"],
        vec!["session", "recover", id],
        vec!["session", "show", id],
        vec!["session", "log", id],
        vec!["session", "export", id],
        vec!["session", "reference", id],
        vec!["session", "gc", id],
        vec!["run", "--session", "00000000", "--", "continue"],
    ] {
        let refused = run_cli(&fixture, &args);
        assert!(!refused.status.success(), "{args:?}");
        assert!(
            refused
                .stderr
                .contains("incompatible plak build (state r1.e1.p1.t1)"),
            "{args:?}: {}",
            refused.stderr
        );
        assert!(
            refused.stderr.contains("plak session export"),
            "{}",
            refused.stderr
        );
    }
}

#[test]
fn uncertain_mcp_call_recovers_across_three_cli_runs() {
    use plak::store::session_records::SessionRecordStore as _;

    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"tool": {"name": "mcp__fake__escaped_result", "arguments": {}}}, "finish": "tool_calls"}]
    }));
    enable_fake_mcp(&fixture, &[]);
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "call the tool",
        ],
    );
    assert_eq!(first.status.code(), Some(5), "{}", first.stderr);
    let session = first.session_id();
    fixture.write_script(&json!({"version": 1, "steps": [
        {"output": {"text": "continued"}}
    ]}));
    for prompt in ["recover", "third run"] {
        let next = run_cli(&fixture, &["run", "--session", &session, "--", prompt]);
        assert!(next.status.success(), "{}", next.stderr);
        assert!(next.stdout.contains("continued"), "{}", next.stdout);
    }
    let log = run_cli(
        &fixture,
        &["session", "log", &session, "--kind", "tool", "--json"],
    );
    assert!(log.status.success(), "{}", log.stderr);
    let rows: Vec<Value> = log
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        rows.iter()
            .filter(|row| row["kind"] == "tool.started")
            .count(),
        1,
        "{}",
        log.stdout
    );
    let show = run_cli(&fixture, &["session", "show", &session]);
    assert!(show.status.success(), "{}", show.stderr);
    assert!(show.stdout.contains("Tool calls    1"), "{}", show.stdout);
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let views = store.load_views(&session).unwrap().unwrap();
    assert_eq!(
        views
            .rows
            .values()
            .filter(|row| row.as_tool().is_some())
            .count(),
        1
    );
    let recover = run_cli(&fixture, &["session", "recover", &session]);
    assert!(recover.status.success(), "{}", recover.stderr);
}

#[test]
fn mcp_completed_result_mapping_failure_requires_recovery() {
    for code_mode in [false, true] {
        let steps = if code_mode {
            json!([
                {"output": {"cell": "await tools.mcp__fake__escaped_result({});"}, "finish": "tool_calls"},
                {"output": {"text": "must not continue"}}
            ])
        } else {
            json!([
                {"output": {"tool": {"name": "mcp__fake__escaped_result", "arguments": {}}}, "finish": "tool_calls"},
                {"output": {"text": "must not continue"}}
            ])
        };
        let script = if code_mode {
            code_mode_script(&steps)
        } else {
            json!({"version": 1, "steps": steps})
        };
        let fixture = Fixture::new(&script);
        let log = fixture.root.join("mcp-calls.log");
        enable_fake_mcp(&fixture, &["--call-log", log.to_str().expect("log path")]);
        let mut agent = AgentProcess::start(&fixture, &[]);
        agent.wait_for("ready", |record| record["kind"] == "ready");
        agent.send(&prompt_frame("call the tool"));
        agent.wait_for("terminal", |record| {
            record["kind"] == "turn_lifecycle"
                && matches!(
                    record["phase"].as_str(),
                    Some("completed" | "failed" | "recovery_required")
                )
        });
        agent.send(&quit_frame());
        let outcome = agent.finish();
        let terminals = terminal_lifecycles(&outcome);
        assert_eq!(terminals.len(), 1);
        assert_eq!(
            terminals[0].1,
            "recovery_required",
            "{}",
            outcome.lines.join("\n")
        );
        assert_eq!(
            std::fs::read_to_string(&log).expect("call log"),
            "escaped_result\n"
        );
    }
}

#[test]
fn mcp_definite_errors_continue_direct_and_code_mode_turns() {
    for code_mode in [false, true] {
        for (tool, diagnostic) in [
            ("fail", "scripted failure"),
            ("rpc_error", "scripted RPC refusal"),
        ] {
            let name = format!("mcp__fake__{tool}");
            let steps = if code_mode {
                json!([
                    {"output": {"cell": format!(
                        "let caught = false; try {{ await tools.{name}({{}}); }} catch (error) {{ if (!String(error).includes('{diagnostic}')) throw error; caught = true; text(String(error)); }} if (!caught) throw new Error('missing rejection');"
                    )}, "finish": "tool_calls"},
                    {"output": {"text": "continued after definite MCP error"}}
                ])
            } else {
                json!([
                    {"output": {"tool": {"name": name, "arguments": {}}}, "finish": "tool_calls"},
                    {"output": {"text": "continued after definite MCP error"}}
                ])
            };
            let script = if code_mode {
                code_mode_script(&steps)
            } else {
                json!({"version": 1, "steps": steps})
            };
            let fixture = Fixture::new(&script);
            let log = fixture.root.join("mcp-calls.log");
            enable_fake_mcp(&fixture, &["--call-log", log.to_str().expect("log path")]);
            let mut agent = AgentProcess::start(&fixture, &[]);
            agent.wait_for("ready", |record| record["kind"] == "ready");
            agent.send(&prompt_frame("call the failing tool, then continue"));
            agent.wait_for("assistant_done", |record| {
                record["kind"] == "assistant_done"
            });
            agent.send(&quit_frame());
            let outcome = agent.finish();
            let stream = outcome.lines.join("\n");
            assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
            assert!(stream.contains(diagnostic), "stream:\n{stream}");
            assert!(
                stream.contains("continued after definite MCP error"),
                "stream:\n{stream}"
            );
            assert!(!stream.contains("recovery_required"), "stream:\n{stream}");
            let terminals = terminal_lifecycles(&outcome);
            assert_eq!(terminals.len(), 1, "stream:\n{stream}");
            assert_eq!(terminals[0].1, "completed", "stream:\n{stream}");
            if code_mode {
                assert!(
                    completed_tool_names(&outcome)
                        .iter()
                        .any(|name| name == "exec"),
                    "stream:\n{stream}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(&log).expect("call log"),
                format!("{tool}\n")
            );
        }
    }
}

/// An explicit compaction of a session whose history holds a template MCP
/// call commits: the summary binding carries the root's MCP extension, so
/// its tool surface is the source's, and the server sees the call once.
#[test]
fn a_compaction_over_a_template_mcp_call_commits_and_continues() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "maximum_projected_input": 200_000,
        "content_sensitive_projection": true,
        "fold": "accept",
        "steps": [
            {"output": {"tool": {"name": "mcp__fake__echo", "arguments": {"text": "from-mcp"}}}, "finish": "tool_calls"},
            {"output": {"text": "the first answer"}, "usage": {}},
            {"output": {"text": "the second answer"}, "usage": {}},
            {"output": {"text": "the compacted answer"}, "usage": {}}
        ]
    }));
    write_private(
        &fixture.home().join("config.toml"),
        "[root]\nagent = \"lead\"\n\n[orchestration]\nsubagents = false\n\n[compaction]\nenabled = false\n",
    );
    let log = fixture.root.join("mcp-calls.log");
    enable_fake_mcp(&fixture, &["--call-log", log.to_str().expect("log path")]);
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("call the server"));
    agent.wait_for("the first answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the first answer"
    });
    agent.wait_for_completed_turns(1);
    agent.send(&prompt_frame("answer the second intent"));
    agent.wait_for("the second answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the second answer"
    });
    agent.wait_for_completed_turns(2);
    agent.send(&compact_frame());
    let compact = agent.wait_for("the compaction outcome", |record| {
        record["kind"] == "tool"
            && record["activity_kind"] == "compact"
            && matches!(record["state"].as_str(), Some("done" | "failed"))
    });
    assert_eq!(compact["state"], "done", "the fold committed: {compact}");
    agent.send(&prompt_frame("carry on after the compaction"));
    agent.wait_for("the follow-up answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the compacted answer"
    });
    agent.wait_for_completed_turns(3);
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    assert_compaction_did_not_fail(&outcome);
    assert_eq!(std::fs::read_to_string(&log).expect("call log"), "echo\n");
}

/// §6 quit over a live nested process: Ctrl+Q while a real cell's
/// `exec_command` is still inside its yield must restore the terminal, let
/// the process group's TERM reach the child's trap, and keep the session
/// resumable — the reopened composition proves the evaluator reattached by
/// running a real two-call cell.
#[test]
fn real_composition_quit_releases_nested_process() {
    let fixture = Fixture::new(&code_mode_owned_script());
    let mut guard = OwnedProcessGuard::default();
    let mut terminal = TerminalChild::spawn(&fixture, 100, None);
    terminal.wait_screen("/10M");
    terminal.send(b"run the owned cell\r");
    wait_for_fixture_file(&fixture.workspace().join("owned.pid"));
    guard.arm(&fixture.workspace());

    terminal.send(b"\x11");
    let (status, raw) = terminal.finish_with_raw();
    assert!(
        status.success(),
        "the composition quit did not exit cleanly; cooked output was:\n{}",
        String::from_utf8_lossy(&raw)
    );
    let leave = raw
        .windows(b"\x1b[?1049l".len())
        .position(|window| window == b"\x1b[?1049l")
        .expect("the quit must leave the alternate screen");
    assert!(
        raw_contains(&raw[leave..], b"\x1b[?25h"),
        "the cursor must be shown after the alternate screen is left"
    );
    assert_owned_process_stopped(&fixture.workspace());
    let session = resume_hint_session(&raw)
        .expect("a saved session must print its resume hint to the cooked terminal");
    let before = session_event_files(&fixture);
    assert_eq!(
        before.len(),
        1,
        "one saved session expected, got {before:?}"
    );
    assert!(
        before[0]
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&session)),
        "the resume hint {session} must prefix the saved session record {before:?}"
    );

    fixture.write_script(&code_mode_script(&json!([
        {"output": {"cell": TWO_NESTED_CALLS}, "finish": "tool_calls"},
        {"output": {"text": "the composed answer"}}
    ])));
    let mut resumed = TerminalChild::spawn(&fixture, 100, Some(&session));
    resumed.wait_screen("/ commands");
    resumed.send(b"run nested\r");
    resumed.wait_screen("the composed answer");
    resumed.send(b"\x11");
    assert!(resumed.finish().success());

    assert_eq!(
        session_event_files(&fixture),
        before,
        "the resume must attach the same session, not open a fresh one"
    );
}

/// A clip inside a composed answer is announced once, for the answer.
///
/// The composed answer carries every nested call's output and is the largest
/// result the product produces. The nested composition child deliberately does
/// not announce, because its completion carries no attempt id and the event is
/// call-scoped, so one note for the composed answer is the contract.
///
/// The command must be plain text: a `tr '\0' x` spelling reaches the process
/// tool as a literal NUL, which it refuses, so the cell fails with nothing to
/// clip. Before nested results bypassed the budget, the nested `exec_command`
/// result was clipped and announced and the composed answer was then clipped
/// again to what was left: two notes.
#[test]
fn a_clipped_observation_inside_a_composed_answer_is_announced_once() {
    let mut plan = code_mode_script(&json!([
        {"output": {"cell": "const big = await tools.exec_command({ cmd: \"yes x | head -c 60000\" });\ntext(big);\n"}, "finish": "tool_calls"},
        {"output": {"text": "answered from the kept portion"}}
    ]));
    if let Some(object) = plan.as_object_mut() {
        object.insert(
            "observation_budget_cap_bytes".to_owned(),
            Value::from(2_048),
        );
    }
    let fixture = Fixture::new(&plan);
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("produce a large composed answer"));
    agent.wait_for("the observation clip note", |record| {
        record["kind"] == "status"
            && record["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("observation clipped:"))
    });
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    let notes = outcome
        .records()
        .iter()
        .filter_map(|record| record["detail"].as_str())
        .filter(|detail| detail.contains("observation clipped:"))
        .count();
    assert_eq!(
        notes,
        1,
        "one note for the composed answer, not one per nested call; stream:\n{}",
        outcome.lines.join("\n")
    );
    assert_clip_is_recorded(&fixture);
}

/// A Code Mode session written by one process reopens in another.
///
/// This is the case that catches a per-process plan authority, and it is worth
/// a case of its own rather than a line in another one. The harness plan binds
/// the plan-state authority, so a registry built with
/// `standard_coding_packs_in_memory` compiles a different plan in every
/// process, and the reopen is refused with "builder harness plan disagrees with
/// durable semantics". Nothing in a single-process test can see that: the
/// authority is stable for as long as the process lives.
///
/// crash-matrix found it on the crash-route axis. The Code Mode binding now
/// pins its own plan identity, and this is the case that says so on the face a
/// user would actually hit it on.
#[test]
fn a_code_mode_session_written_by_an_earlier_process_reopens() {
    let fixture = Fixture::new(&code_mode_script(&json!([
        {"output": {"cell": TWO_NESTED_CALLS}, "finish": "tool_calls"},
        {"output": {"text": "the composed answer"}}
    ])));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("run a cell"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let first = agent.finish();
    assert!(first.status.success(), "stderr:\n{}", first.stderr);
    let session = first.session_id();

    // A second cell on the reopened session, so the reopen is proven usable
    // rather than merely accepted. A script that only answered with text would
    // pass even if the composition attachment had been lost.
    fixture.write_script(&code_mode_script(&json!([
        {"output": {"cell": "const again = await tools.exec_command({ cmd: \"printf reopened\" });\nif (again.output !== \"reopened\" || again.exit_code !== 0) throw new Error(\"bad reopened process object\"); text(again.output);\n"}, "finish": "tool_calls"},
        {"output": {"text": "answered after the reopen"}}
    ])));
    let mut resumed = AgentProcess::start(&fixture, &["--session", &session]);
    resumed.wait_for("ready", |record| record["kind"] == "ready");
    resumed.send(&prompt_frame("run another cell"));
    resumed.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    resumed.send(&quit_frame());
    let outcome = resumed.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(outcome.session_id(), session);
    assert_eq!(
        outcome.assistant_text(),
        vec!["answered after the reopen".to_owned()]
    );
    let nested = nested_call_names(&outcome, &outer_call_id(&outcome, "exec"));
    assert_eq!(
        nested.len(),
        1,
        "the reopened session's cell never ran its nested call, saw {nested:?}; stream:\n{}",
        outcome.lines.join("\n")
    );
}

/// A turn that spawns a child, waits for it, and adopts what it produced.
///
/// This is the only route from a spawned process to a real child. An injected
/// runtime binding replaces the whole tool registry, so unless the topology
/// tools *are* the registry, `spawn_agent` does not exist for the model to
/// call; the `tree` dial is what makes them the registry. The three rounds are
/// one turn: nothing here is a fresh prompt.
///
/// The script never restates an id the tree minted. `$result.agent_id` and
/// `$result.candidate` read the previous round's tool result, so the case
/// drives a real tree rather than asserting the tree's id shapes.
#[test]
fn a_turn_spawns_a_child_waits_for_it_and_adopts_its_candidate() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "tree": true,
        "steps": [
            {"output": {"tool": {"name": "spawn_agent", "arguments": {
                "template": "implement",
                "tier": "mutate",
                "workspace": "fork",
                "description": "offline scripted child",
                "prompt": "make the offline scripted change",
                "output_schema_json": "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
            }}}, "finish": "tool_calls"},
            {"output": {"tool": {"name": "wait_agent", "arguments": {
                "agent_ids": ["$result.agent_id"]
            }}}, "finish": "tool_calls"},
            {"output": {"tool": {"name": "adopt_candidate", "arguments": {
                "candidate_id": "$result.candidate"
            }}}, "finish": "tool_calls"},
            {"output": {"text": "the child's work is adopted"}}
        ]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("delegate this"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(
        outcome.status.success(),
        "stderr:
{}",
        outcome.stderr
    );
    outcome.assert_exactly_one_terminal_record();
    assert_eq!(
        outcome.assistant_text(),
        vec!["the child's work is adopted".to_owned()]
    );
    // All three rounds completed, in order. A tree that spawned but never
    // adopted would still reach the final text, so naming the whole sequence
    // is what makes this a tree case rather than a spawn case.
    assert!(outcome.lines.iter().any(|line| {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        record["kind"] == "tool"
            && record["name"] == "child_result"
            && record["activity_kind"] == "agent"
            && record["subject"] == "m1 · c1 completed"
    }));
    assert_eq!(
        completed_tool_names(&outcome)
            .into_iter()
            .filter(|name| name != "child_result")
            .collect::<Vec<_>>(),
        vec![
            "spawn_agent".to_owned(),
            "wait_agent".to_owned(),
            "adopt_candidate".to_owned()
        ],
        "stream:
{}",
        outcome.lines.join(
            "
"
        )
    );
    // The placeholders resolved. An unresolved `$result.` reference is passed
    // through as written, so it would reach the tool and be refused there;
    // asserting its absence catches a substitution that silently did nothing.
    assert!(
        !outcome
            .lines
            .join(
                "
"
            )
            .contains("$result."),
        "a tree placeholder was never resolved; stream:
{}",
        outcome.lines.join(
            "
"
        )
    );
}

/// The three spellings that name no particular cell (spec 9.4).
const LOWERED_WAIT_SPELLINGS: [&str; 3] = ["none", "null", "all"];

/// A `wait` naming no cell, with nothing running, is a typed empty success.
#[test]
fn a_wait_that_names_no_cell_succeeds_emptily_when_nothing_is_running() {
    for spelling in LOWERED_WAIT_SPELLINGS {
        let fixture = Fixture::new(&code_mode_script(&json!([
            {"output": {"wait": {"cell_id": spelling}}, "finish": "tool_calls"},
            {"output": {"text": "answered after the empty wait"}}
        ])));
        let mut agent = AgentProcess::start(&fixture, &[]);
        agent.wait_for("ready", |record| record["kind"] == "ready");
        agent.send(&prompt_frame("wait on nothing"));
        agent.wait_for("assistant_done", |record| {
            record["kind"] == "assistant_done"
        });
        agent.send(&quit_frame());
        let outcome = agent.finish();
        assert!(
            outcome.status.success(),
            "`{spelling}` must be answered rather than refused; stderr:\n{}",
            outcome.stderr
        );
        assert_eq!(
            outcome.assistant_text(),
            vec!["answered after the empty wait".to_owned()],
            "`{spelling}` did not complete the turn"
        );
    }
}

/// A `wait` naming no cell lowers onto the cell that is running.
///
/// Budget lane: the cell sleeps for three seconds on purpose, so the wait has
/// something to lower onto.
#[test]
fn a_wait_that_names_no_cell_lowers_onto_a_running_cell() {
    for spelling in LOWERED_WAIT_SPELLINGS {
        let fixture = Fixture::new(&code_mode_script(&json!([
            {"output": {"cell": "await tools.exec_command({ cmd: \"sleep 3\" });\ntext(\"slept\");\n"}, "finish": "tool_calls"},
            {"output": {"wait": {"cell_id": spelling, "yield_time_ms": 10_000}}, "finish": "tool_calls"},
            {"output": {"text": "answered after the lowered wait"}}
        ])));
        let mut agent = AgentProcess::start(&fixture, &[]);
        agent.wait_for("ready", |record| record["kind"] == "ready");
        agent.send(&prompt_frame("wait on the running cell"));
        agent.wait_for("assistant_done", |record| {
            record["kind"] == "assistant_done"
        });
        agent.send(&quit_frame());
        let outcome = agent.finish();
        assert!(
            outcome.status.success(),
            "`{spelling}` must lower onto the running cell; stderr:\n{}",
            outcome.stderr
        );
        assert_eq!(
            outcome.assistant_text(),
            vec!["answered after the lowered wait".to_owned()],
            "`{spelling}` did not complete the turn"
        );
    }
}

// ---------------------------------------------------------------------------
// Terminal application
// ---------------------------------------------------------------------------

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

const TUI_ROWS: u16 = 30;

/// One real terminal running the shipped binary on a pseudoterminal.
struct TerminalChild {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    capture: Arc<Mutex<TerminalCapture>>,
    reader: Option<std::thread::JoinHandle<()>>,
    cols: u16,
}

struct TerminalCapture {
    parser: vt100::Parser,
    raw: Vec<u8>,
}

impl TerminalCapture {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            raw: Vec::new(),
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.raw.extend_from_slice(bytes);
    }
}

impl TerminalChild {
    fn spawn(fixture: &Fixture, cols: u16, resume: Option<&str>) -> Self {
        Self::spawn_with_pilot(fixture, cols, resume, None)
    }

    fn spawn_with_pilot(
        fixture: &Fixture,
        cols: u16,
        resume: Option<&str>,
        pilot: Option<&Path>,
    ) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: TUI_ROWS,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pty opened");
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_plak"));
        match resume {
            Some("") => command.args(["--resume"]),
            Some(session) => command.args(["--resume", session]),
            None => {}
        }
        command.env_clear();
        command.env("PATH", std::env::var("PATH").unwrap_or_default());
        command.env("HOME", fixture.root.join("fake-home"));
        command.env("TMPDIR", fixture.root.join("tmp"));
        command.env("PLAK_HOME", fixture.home());
        command.env("PLAK_TEST_MODEL", "offline-loopback-v1");
        command.env("PLAK_TEST_SCRIPT", fixture.script());
        command.env("PLAK_TEST_EVIDENCE", fixture.root.join("evidence.jsonl"));
        if let Some(pilot) = pilot {
            command.env("PLAK_TEST_TUI_PILOT", pilot);
        }
        for name in ["LLVM_PROFILE_FILE", "CARGO_LLVM_COV_TARGET_DIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        command.env("LANG", "C.UTF-8");
        command.cwd(fixture.workspace());
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = pair.master.take_writer().expect("pty writer");
        let child = pair.slave.spawn_command(command).expect("tui spawned");
        drop(pair.slave);
        let capture = Arc::new(Mutex::new(TerminalCapture::new(TUI_ROWS, cols)));
        let sink = Arc::clone(&capture);
        let reader_thread = std::thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                sink.lock().unwrap().process(&buffer[..count]);
            }
        });
        Self {
            master: pair.master,
            writer,
            child,
            capture,
            reader: Some(reader_thread),
            cols,
        }
    }

    fn screen(&self) -> String {
        self.capture.lock().unwrap().parser.screen().contents()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    /// Waits for a stable frame matching `predicate`.
    fn wait_where(&mut self, description: &str, predicate: impl Fn(&str) -> bool) -> String {
        self.wait_where_for(description, WAIT, predicate)
    }

    fn wait_where_for(
        &mut self,
        description: &str,
        timeout: Duration,
        predicate: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let first = self.screen();
            if predicate(&first) {
                std::thread::sleep(Duration::from_millis(40));
                let second = self.screen();
                if first == second {
                    return second;
                }
            }
            if let Some(status) = self.child.try_wait().expect("wait") {
                panic!("the terminal exited before stable {description}: {status:?}\n{first}");
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for stable {description}\n{first}"
            );
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn wait_screen(&mut self, needle: &str) -> String {
        self.wait_where(&format!("a screen containing `{needle}`"), |frame| {
            frame.contains(needle)
        })
    }

    fn observation(&self) -> TerminalObservation {
        TerminalObservation::capture(&self.capture.lock().unwrap(), TUI_ROWS, self.cols)
    }

    fn finish(self) -> portable_pty::ExitStatus {
        self.finish_with_raw().0
    }

    fn finish_with_raw(mut self) -> (portable_pty::ExitStatus, Vec<u8>) {
        let deadline = Instant::now() + WAIT;
        // A quit that lands while a turn still settles asks first (spec 02
        // §5.3): confirm it once, as the operator would.
        let mut confirmed = false;
        let status = loop {
            match self.child.try_wait().expect("wait") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    panic!("the terminal never exited\n{}", self.screen());
                }
                None if !confirmed && self.screen().contains("Quit plak?") => {
                    confirmed = true;
                    // Focus starts on Quit, so Enter confirms. Best effort:
                    // the test may have confirmed already and the TUI may be
                    // gone; macOS reports a write to a closed pty as EIO.
                    let _ = self
                        .writer
                        .write_all(b"\r")
                        .and_then(|()| self.writer.flush());
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        drop(self.master);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let raw = self.capture.lock().unwrap().raw.clone();
        (status, raw)
    }
}

struct PilotClient {
    root: PathBuf,
    next_sequence: u64,
    revision: Option<u64>,
    snapshot: Option<Value>,
}

impl PilotClient {
    fn new(root: PathBuf) -> Self {
        std::fs::create_dir(&root).expect("pilot directory");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("pilot directory mode");
        Self {
            root,
            next_sequence: 1,
            revision: None,
            snapshot: None,
        }
    }

    fn observe(&mut self) -> Value {
        self.exchange("observe", None, false)
    }

    /// Polls `observe` at the boundary until `predicate` holds — the explicit
    /// wait the caller owns, never a blind sleep inside the driver.
    fn observe_until(&mut self, what: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let observed = self.exchange("observe", None, false);
            assert_eq!(observed["status"], "observed");
            if predicate(&observed) {
                return observed;
            }
            assert!(
                Instant::now() < deadline,
                "{what} never observed; last snapshot was {observed}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn act(&mut self, action: &str, text: Option<&str>) -> Value {
        let deadline = Instant::now() + WAIT;
        for _ in 0..10 {
            let precondition = self.snapshot.clone().expect("observation before mutating");
            let document = self.exchange(action, text, true);
            if document["status"] != "stale_observation" {
                return document;
            }
            assert_eq!(document["error"], Value::Null);
            assert!(
                Instant::now() < deadline,
                "pilot action {action} never found a current revision"
            );
            let observed = self.exchange("observe", None, false);
            assert_eq!(observed["status"], "observed");
            // A retry is only safe while the request still means the same
            // thing: the action's target state must not have changed
            // underneath the stale fence.
            for key in ["owner", "binding", "composer", "modal", "queue"] {
                assert_eq!(
                    observed["snapshot"][key], precondition[key],
                    "stale retry must not silently change the action target: {key}"
                );
            }
        }
        panic!("pilot action {action} never found a current revision");
    }

    fn exchange(&mut self, action: &str, text: Option<&str>, mutate: bool) -> Value {
        let sequence = self.next_sequence;
        let request = self.root.join(format!("request-{sequence:08}.json"));
        let response = self.root.join(format!("response-{sequence:08}.json"));
        let temporary = request.with_extension("tmp");
        let mut document = json!({
            "schema": "plak.tui-pilot.v2",
            "sequence": sequence,
            "action": action,
            "text": text,
        });
        if mutate {
            document["if_revision"] = json!(self.revision.expect("observe before mutating"));
        }
        write_private(
            &temporary,
            &serde_json::to_string(&document).expect("pilot request JSON"),
        );
        std::fs::rename(&temporary, &request).expect("publish pilot request");
        let deadline = Instant::now() + WAIT;
        while !response.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for pilot response {sequence}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let document: Value =
            serde_json::from_slice(&std::fs::read(response).expect("pilot response readable"))
                .expect("pilot response JSON");
        assert_eq!(document["schema"], "plak.tui-pilot.v2");
        assert_eq!(document["sequence"], sequence);
        self.revision = document["revision"].as_u64().or(self.revision);
        self.snapshot = Some(document["snapshot"].clone());
        self.next_sequence += 1;
        document
    }
}

fn pilot_action_ids(response: &Value) -> Vec<&str> {
    response["legal_actions"]
        .as_array()
        .expect("legal action array")
        .iter()
        .map(|action| action["id"].as_str().expect("legal action id"))
        .collect()
}

#[test]
fn semantic_pilot_sidecar_drives_the_real_tui_and_host_mailbox() {
    let streamed = "streamed evidence ".repeat(1_024);
    // Streamed deltas are a prefix of the terminal response: the runtime
    // emits only the missing suffix once the authoritative text lands.
    let second_answer = format!("{streamed}second streamed answer");
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let release = fixture.root.join("release-second-answer");
    fixture.write_script(&json!({
        "version": 1,
        "steps": [
            {"output": {"text": "semantic pilot answer"}},
            {"prefix": streamed, "output": {"text": second_answer}, "gate": {"file": release}}
        ]
    }));
    let mut pilot = PilotClient::new(fixture.root.join("pilot"));
    let mut terminal = TerminalChild::spawn_with_pilot(&fixture, 100, None, Some(&pilot.root));
    terminal.wait_screen("/10M");

    let initial = pilot.observe();
    assert_eq!(initial["error"], Value::Null);
    assert_eq!(initial["status"], "observed");
    assert_eq!(initial["host_action_pending"], false);
    assert_eq!(initial["snapshot"]["owner"], "composer");
    assert!(pilot_action_ids(&initial).contains(&"open-settings"));

    let settings = pilot.act("open-settings", None);
    assert_eq!(settings["error"], Value::Null);
    assert_eq!(settings["status"], "applied");
    assert_eq!(settings["snapshot"]["owner"], "settings");
    terminal.wait_screen("Settings");

    let composer = pilot.act("close-focused-owner", None);
    assert_eq!(composer["snapshot"]["owner"], "composer");
    let draft = pilot.act("type-text", Some("semantic pilot prompt"));
    assert_eq!(draft["status"], "applied");
    assert_eq!(draft["snapshot"]["composer"]["char_count"], 21);
    assert!(!pilot_action_ids(&draft).contains(&"open-slash"));

    pilot.observe_until("the bound agent", |document| {
        document["snapshot"]["binding"]["attached"] == true
    });
    let sent = pilot.act("send", None);
    assert_eq!(sent["error"], Value::Null);
    assert_eq!(sent["status"], "applied");
    assert_eq!(
        sent["host_action_pending"], true,
        "send produced a host intent that has not been dispatched yet"
    );
    assert_eq!(sent["snapshot"]["composer"]["char_count"], 0);
    assert_eq!(
        sent["snapshot"]["turn"],
        Value::Null,
        "a pending host intent is not a terminal outcome"
    );
    terminal.wait_screen("semantic pilot answer");
    // The answer streams onto the screen before the turn commits, so the
    // screen is not the terminal fence; idle is.
    let finished = pilot.observe_until("the first turn going idle", |document| {
        document["snapshot"]["phase"] == "idle"
    });
    assert_eq!(finished["snapshot"]["turn"]["phase"], "completed");
    assert!(finished["snapshot"]["transcript"]["block_count"].as_u64() >= Some(2));
    let first_turn = finished["snapshot"]["turn"]["turn_id"]
        .as_str()
        .expect("first turn id")
        .to_owned();

    pilot_burst_preserves_the_parked_draft_and_turn(
        &mut pilot,
        &mut terminal,
        &first_turn,
        &release,
    );

    let quit = pilot.act("quit", None);
    assert_eq!(quit["error"], Value::Null);
    assert_eq!(quit["status"], "applied");
    assert!(terminal.finish().success());
}

/// Observation pressure: a second turn whose 16KB streamed prefix bursts
/// host events while the settings modal owns the screen and a private
/// draft sits parked in the composer. The scripted gate holds the turn in
/// progress until the modal owns the parked draft, and the burst must not
/// split the turn, lose the draft, or let a stale fence mutate.
fn pilot_burst_preserves_the_parked_draft_and_turn(
    pilot: &mut PilotClient,
    terminal: &mut TerminalChild,
    first_turn: &str,
    release: &Path,
) {
    let drafted_second = pilot.act("type-text", Some("second prompt"));
    assert_eq!(drafted_second["status"], "applied");
    let sent_second = pilot.act("send", None);
    assert_eq!(sent_second["status"], "applied");
    let opened = pilot.act("open-settings", None);
    assert_eq!(opened["snapshot"]["owner"], "settings");
    assert_eq!(
        opened["snapshot"]["busy"], true,
        "the stalled turn must still be in progress when the modal opens"
    );
    let drafting = pilot.act("close-focused-owner", None);
    assert_eq!(drafting["snapshot"]["owner"], "composer");
    let draft = pilot.act("type-text", Some("kept draft"));
    assert_eq!(draft["snapshot"]["composer"]["char_count"], 10);
    let modal = pilot.act("open-settings", None);
    assert_eq!(modal["snapshot"]["owner"], "settings");
    assert_eq!(
        modal["snapshot"]["composer"]["char_count"], 10,
        "a modal owns the key chain over a parked draft, never clears it"
    );
    std::fs::write(release, b"release").unwrap();

    let completed = pilot.observe_until("the second turn's completed marker", |document| {
        document["snapshot"]["turn"]["phase"] == "completed"
            && document["snapshot"]["turn"]["turn_id"] != first_turn
    });
    let second_turn = completed["snapshot"]["turn"]["turn_id"]
        .as_str()
        .expect("second turn id")
        .to_owned();
    assert_ne!(
        second_turn, first_turn,
        "the second admitted turn must carry its own canonical id"
    );
    assert_eq!(completed["snapshot"]["owner"], "settings");
    assert_eq!(
        completed["snapshot"]["composer"]["char_count"], 10,
        "the private draft must survive the whole host event burst"
    );

    let composer = pilot.act("close-focused-owner", None);
    assert_eq!(composer["snapshot"]["owner"], "composer");
    assert_eq!(
        composer["snapshot"]["composer"]["char_count"], 10,
        "the private draft must survive the host event burst and the modal"
    );
    assert_eq!(composer["snapshot"]["turn"]["turn_id"], second_turn);
    assert_eq!(composer["snapshot"]["turn"]["phase"], "completed");
    terminal.wait_screen("second streamed answer");
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct TerminalObservation {
    rows: u16,
    cols: u16,
    screen: String,
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    style_runs: Vec<StyleRun>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct StyleRun {
    row: u16,
    start_col: u16,
    end_col: u16,
    foreground: String,
    background: String,
    attributes: Vec<&'static str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CellStyle {
    foreground: String,
    background: String,
    attributes: u8,
}

impl CellStyle {
    const BOLD: u8 = 1;
    const ITALIC: u8 = 1 << 1;
    const UNDERLINE: u8 = 1 << 2;
    const INVERSE: u8 = 1 << 3;

    fn from_cell(cell: &vt100::Cell) -> Self {
        let mut attributes = 0;
        if cell.bold() {
            attributes |= Self::BOLD;
        }
        if cell.italic() {
            attributes |= Self::ITALIC;
        }
        if cell.underline() {
            attributes |= Self::UNDERLINE;
        }
        if cell.inverse() {
            attributes |= Self::INVERSE;
        }
        Self {
            foreground: format!("{:?}", cell.fgcolor()),
            background: format!("{:?}", cell.bgcolor()),
            attributes,
        }
    }

    fn is_default(&self) -> bool {
        self.foreground == "Default" && self.background == "Default" && self.attributes == 0
    }

    fn attribute_names(&self) -> Vec<&'static str> {
        [
            (Self::BOLD, "bold"),
            (Self::ITALIC, "italic"),
            (Self::UNDERLINE, "underline"),
            (Self::INVERSE, "inverse"),
        ]
        .into_iter()
        .filter_map(|(flag, name)| (self.attributes & flag != 0).then_some(name))
        .collect()
    }
}

impl TerminalObservation {
    fn capture(capture: &TerminalCapture, rows: u16, cols: u16) -> Self {
        let screen = capture.parser.screen();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let mut style_runs = Vec::new();
        for row in 0..rows {
            let mut active: Option<(u16, CellStyle)> = None;
            for col in 0..=cols {
                let style = (col < cols)
                    .then(|| screen.cell(row, col))
                    .flatten()
                    .map(CellStyle::from_cell)
                    .filter(|style| !style.is_default());
                match (&active, style) {
                    (Some((_, current)), Some(next)) if *current == next => {}
                    (Some((start, current)), next) => {
                        style_runs.push(StyleRun {
                            row,
                            start_col: *start,
                            end_col: col,
                            foreground: current.foreground.clone(),
                            background: current.background.clone(),
                            attributes: current.attribute_names(),
                        });
                        active = next.map(|style| (col, style));
                    }
                    (None, Some(next)) => active = Some((col, next)),
                    (None, None) => {}
                }
            }
        }
        Self {
            rows,
            cols,
            screen: screen.contents(),
            cursor_row,
            cursor_col,
            cursor_visible: !screen.hide_cursor(),
            style_runs,
        }
    }
}

/// A launch paints its banner, its bound tuple and its composer at every width.
///
/// The banner declines to paint only in a pane too narrow to hold it, so the
/// wordmark is asserted at the two widths that admit it and the composer at
/// all three.
#[test]
fn the_terminal_launches_with_its_banner_at_three_widths() {
    for cols in [60_u16, 100, 160] {
        let fixture = Fixture::new(&two_tool_round_trips("the offline answer is 42"));
        let mut terminal = TerminalChild::spawn(&fixture, cols, None);
        let frame = terminal.wait_screen("/10M");
        assert!(
            frame.contains("/ commands"),
            "the composer is missing at {cols} columns:\n{frame}"
        );
        assert!(
            frame.lines().count() <= usize::from(TUI_ROWS) + 1,
            "the first frame overflowed the pane at {cols} columns:\n{frame}"
        );
        if cols >= 100 {
            assert!(
                frame.contains("powered by plakous"),
                "the launch banner is missing at {cols} columns:\n{frame}"
            );
        }
        terminal.send(b"/quit\r");
        let status = terminal.finish();
        assert!(
            status.success(),
            "the terminal did not exit cleanly at {cols} columns"
        );
    }
}

/// A scripted turn renders its tool rows and its answer in the transcript.
#[test]
fn a_terminal_turn_renders_its_tool_call_and_answer() {
    let fixture = Fixture::new(&two_tool_round_trips("the offline answer is 42"));
    let mut terminal = TerminalChild::spawn(&fixture, 100, None);
    terminal.wait_screen("/10M");
    terminal.send(b"do the thing\r");
    let frame = terminal.wait_screen("the offline answer is 42");
    assert!(
        frame.contains("Loopback effect ×2"),
        "the completed activities are grouped: {frame}"
    );
    terminal.send(b"\x0f");
    let frame = terminal.wait_screen("Loopback effect");
    assert!(
        frame.to_lowercase().contains("loopback effect") || frame.contains("loopback_effect"),
        "the transcript did not render the tool call:\n{frame}"
    );
    terminal.send(b"/quit\r");
    assert!(terminal.finish().success());
}

/// Scrolling to the top of a long transcript and back returns to the tail.
#[test]
fn the_transcript_scrolls_to_the_top_and_back_down() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": (0..60)
            .map(|line| format!("transcript line {line}"))
            .collect::<Vec<_>>()
            .join("\n")}}]
    }));
    let mut terminal = TerminalChild::spawn(&fixture, 100, None);
    terminal.wait_screen("/10M");
    terminal.send(b"fill the transcript\r");
    let tail = terminal.wait_screen("transcript line 59");
    // Wheel up until the head of the answer is on screen.
    let deadline = Instant::now() + WAIT;
    loop {
        if terminal.screen().contains("transcript line 0") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the transcript never scrolled to its head:\n{}",
            terminal.screen()
        );
        terminal.send(b"\x1b[<64;10;10M");
        std::thread::sleep(Duration::from_millis(60));
    }
    let head = terminal.wait_screen("transcript line 0");
    assert_ne!(head, tail, "scrolling up did not move the transcript");
    loop {
        if terminal.screen().contains("transcript line 59") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the transcript never returned to its tail:\n{}",
            terminal.screen()
        );
        terminal.send(b"\x1b[<65;10;10M");
        std::thread::sleep(Duration::from_millis(60));
    }
    terminal.send(b"/quit\r");
    assert!(terminal.finish().success());
}

/// A session `plak agent` wrote reopens in the TUI with that turn on screen.
#[test]
fn a_jsonl_session_resumes_in_the_tui_with_its_transcript() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "answer from the agent session"}}]
    }));
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("agent session prompt"));
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });
    agent.send(&quit_frame());
    let first = agent.finish();
    assert!(first.status.success(), "stderr:\n{}", first.stderr);
    let session = first.session_id();

    let mut terminal = TerminalChild::spawn(&fixture, 100, Some(&session));
    terminal.wait_where("the agent transcript is on screen", |frame| {
        frame.contains("agent session prompt") && frame.contains("answer from the agent session")
    });
    terminal.send(b"/quit\r");
    assert!(terminal.finish().success());
}

/// A session an earlier terminal wrote reopens, and its transcript is restored.
#[test]
fn the_terminal_resumes_a_session_an_earlier_process_wrote() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "the first terminal answer"}}]
    }));
    let mut first = TerminalChild::spawn(&fixture, 100, None);
    first.wait_screen("/10M");
    first.send(b"first terminal prompt\r");
    first.wait_screen("the first terminal answer");
    first.send(b"/quit\r");
    assert!(first.finish().success());

    // The resume picker is the product's own history navigator: it labels the
    // sessions this home holds, and the note it carries is the one the earlier
    // process wrote.
    let mut picker = TerminalChild::spawn(&fixture, 100, Some(""));
    let listing = picker.wait_screen("Resume session");
    assert!(
        listing.contains("first terminal prompt") || listing.contains("offline-loopback-v1"),
        "the resume listing did not label the earlier session:\n{listing}"
    );
    // The picker's `Enter` acts once it has been open for 600 ms.
    std::thread::sleep(Duration::from_millis(700));
    picker.send(b"\r");
    // The restored transcript carries the earlier process's answer.
    picker.wait_screen("the first terminal answer");
    picker.send(b"second terminal prompt\r");
    // A second turn lands on the reopened session: the same scripted answer,
    // now twice. The script is read once per process, so the reopened process
    // replays round one — the second occurrence is the new turn.
    let frame = picker.wait_where("two answered turns", |frame| {
        frame.matches("the first terminal answer").count() >= 2
    });
    assert!(
        frame.contains("second terminal prompt"),
        "the reopened session did not accept the second prompt:\n{frame}"
    );
    picker.send(b"/quit\r");
    assert!(picker.finish().success());
}

/// The terminal renders a Code Mode turn: the cell and its composed answer.
#[test]
fn the_terminal_renders_a_code_mode_turn() {
    let fixture = Fixture::new(&code_mode_script(&json!([
        {"output": {"cell": TWO_NESTED_CALLS}, "finish": "tool_calls"},
        {"output": {"text": "the composed answer"}}
    ])));
    let mut terminal = TerminalChild::spawn(&fixture, 100, None);
    terminal.wait_screen("/10M");
    terminal.send(b"run a cell\r");
    let frame = terminal.wait_screen("the composed answer");
    assert!(frame.contains("CodeMode · 2 calls"), "{frame}");
    terminal.send(b"\x0f");
    let frame = terminal.wait_screen("$ printf nested-two");
    for expected in ["$ printf nested-one", "$ printf nested-two"] {
        assert!(
            frame.contains(expected),
            "the expanded cell did not render {expected}: {frame}"
        );
    }
    terminal.send(b"/quit\r");
    assert!(terminal.finish().success());
}

// ---------------------------------------------------------------------------
// Declarative real-PTY stories
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TuiStory {
    schema: String,
    name: String,
    terminal: StoryTerminal,
    model_script: Value,
    #[serde(default)]
    workspace_files: BTreeMap<String, String>,
    steps: Vec<StoryStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoryTerminal {
    rows: u16,
    cols: u16,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoryStep {
    Wait {
        id: String,
        contains: String,
        #[serde(default = "default_story_timeout_ms")]
        timeout_ms: u64,
    },
    Send {
        id: String,
        text: String,
    },
    Choose {
        id: String,
        instructions: String,
        actions: BTreeMap<String, StoryAction>,
        selected: String,
    },
    Assert {
        id: String,
        #[serde(default)]
        contains: Vec<String>,
        #[serde(default)]
        excludes: Vec<String>,
        cursor_visible: Option<bool>,
    },
    Snapshot {
        id: String,
    },
    Quit {
        id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoryAction {
    description: String,
    input: String,
}

#[derive(Debug, Serialize)]
struct StoryTrace {
    schema: &'static str,
    story: String,
    status: &'static str,
    steps: Vec<StoryTraceStep>,
    terminal_restoration: TerminalRestoration,
}

#[derive(Debug, Serialize)]
struct StoryTraceStep {
    id: String,
    kind: &'static str,
    elapsed_ms: u64,
    before: TerminalObservation,
    after: TerminalObservation,
    choice: Option<StoryChoiceTrace>,
}

#[derive(Debug, Serialize)]
struct StoryChoiceTrace {
    instructions: String,
    actions: BTreeMap<String, StoryAction>,
    selected: String,
}

#[derive(Debug, Default, Serialize)]
struct TerminalRestoration {
    mouse_capture_disabled: bool,
    sgr_mouse_disabled: bool,
    alternate_screen_left: bool,
}

fn default_story_timeout_ms() -> u64 {
    10_000
}

fn story_step_id(step: &StoryStep) -> &str {
    match step {
        StoryStep::Wait { id, .. }
        | StoryStep::Send { id, .. }
        | StoryStep::Choose { id, .. }
        | StoryStep::Assert { id, .. }
        | StoryStep::Snapshot { id }
        | StoryStep::Quit { id } => id,
    }
}

fn story_step_kind(step: &StoryStep) -> &'static str {
    match step {
        StoryStep::Wait { .. } => "wait",
        StoryStep::Send { .. } => "send",
        StoryStep::Choose { .. } => "choose",
        StoryStep::Assert { .. } => "assert",
        StoryStep::Snapshot { .. } => "snapshot",
        StoryStep::Quit { .. } => "quit",
    }
}

fn raw_contains(raw: &[u8], needle: &[u8]) -> bool {
    raw.windows(needle.len()).any(|window| window == needle)
}

fn assert_story_observation(
    id: &str,
    observation: &TerminalObservation,
    contains: &[String],
    excludes: &[String],
    cursor_visible: Option<bool>,
) {
    for needle in contains {
        assert!(
            observation.screen.contains(needle),
            "story step `{id}` did not contain `{needle}`:\n{}",
            observation.screen
        );
    }
    for needle in excludes {
        assert!(
            !observation.screen.contains(needle),
            "story step `{id}` unexpectedly contained `{needle}`:\n{}",
            observation.screen
        );
    }
    if let Some(expected) = cursor_visible {
        assert_eq!(
            observation.cursor_visible, expected,
            "story step `{id}` cursor visibility"
        );
    }
}

fn install_story_workspace(fixture: &Fixture, files: &BTreeMap<String, String>) {
    for (relative, contents) in files {
        let path = Path::new(relative);
        assert!(
            !path.is_absolute()
                && path
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
            "story workspace path must be a plain relative path: {relative}"
        );
        let destination = fixture.workspace().join(path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).expect("story workspace parent");
        }
        std::fs::write(destination, contents).expect("story workspace file");
    }
}

fn apply_story_step(
    terminal: &mut TerminalChild,
    step: &StoryStep,
    before: &TerminalObservation,
) -> Option<StoryChoiceTrace> {
    match step {
        StoryStep::Wait {
            contains,
            timeout_ms,
            ..
        } => {
            terminal.wait_where_for(
                &format!("story screen containing `{contains}`"),
                Duration::from_millis(*timeout_ms),
                |screen| screen.contains(contains),
            );
            None
        }
        StoryStep::Send { text, .. } => {
            terminal.send(text.as_bytes());
            std::thread::sleep(Duration::from_millis(40));
            None
        }
        StoryStep::Choose {
            instructions,
            actions,
            selected,
            ..
        } => {
            assert!(
                !instructions.trim().is_empty(),
                "choice instructions must not be empty"
            );
            assert!(actions.len() >= 2, "choice must offer at least two actions");
            assert!(
                actions
                    .values()
                    .all(|action| !action.description.trim().is_empty()),
                "choice action descriptions must not be empty"
            );
            let action = actions.get(selected).unwrap_or_else(|| {
                panic!("selected action `{selected}` is absent from the closed action table")
            });
            terminal.send(action.input.as_bytes());
            std::thread::sleep(Duration::from_millis(40));
            Some(StoryChoiceTrace {
                instructions: instructions.clone(),
                actions: actions.clone(),
                selected: selected.clone(),
            })
        }
        StoryStep::Assert {
            id,
            contains,
            excludes,
            cursor_visible,
        } => {
            assert_story_observation(id, before, contains, excludes, *cursor_visible);
            None
        }
        StoryStep::Snapshot { .. } => None,
        StoryStep::Quit { .. } => {
            terminal.send(b"/quit\r");
            None
        }
    }
}

/// Executes one JSON-authored story against the real terminal application.
///
/// This is ignored because it is a maintainer driver rather than a fixed test.
/// `PLAK_TUI_STORY` names the input and `PLAK_TUI_TRACE` names the private
/// output under `target/`; neither file is part of the offline gate.
#[test]
#[ignore = "maintainer-supplied declarative TUI story"]
fn run_declarative_tui_story() {
    let story_path = PathBuf::from(
        std::env::var_os("PLAK_TUI_STORY").expect("PLAK_TUI_STORY names a story JSON file"),
    );
    let trace_path = PathBuf::from(
        std::env::var_os("PLAK_TUI_TRACE").expect("PLAK_TUI_TRACE names the output JSON file"),
    );
    let story: TuiStory =
        serde_json::from_slice(&std::fs::read(&story_path).expect("story file readable"))
            .expect("valid story JSON");
    assert_eq!(story.schema, "plak.tui-story.v1", "story schema");
    assert_eq!(story.terminal.rows, TUI_ROWS, "story terminal rows");
    assert!(!story.steps.is_empty(), "story has steps");
    assert!(
        matches!(story.steps.last(), Some(StoryStep::Quit { .. })),
        "story ends with a quit step"
    );

    let fixture = Fixture::new(&story.model_script);
    install_story_workspace(&fixture, &story.workspace_files);
    let mut terminal = TerminalChild::spawn(&fixture, story.terminal.cols, None);
    let mut trace_steps = Vec::with_capacity(story.steps.len());
    let mut finished = None;

    for step in &story.steps {
        let before = terminal.observation();
        let started = Instant::now();
        let choice = apply_story_step(&mut terminal, step, &before);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let after = terminal.observation();
        trace_steps.push(StoryTraceStep {
            id: story_step_id(step).to_owned(),
            kind: story_step_kind(step),
            elapsed_ms,
            before,
            after,
            choice,
        });
        if matches!(step, StoryStep::Quit { .. }) {
            finished = Some(terminal.finish_with_raw());
            break;
        }
    }

    let (status, raw) = finished.expect("story quit was executed");
    assert!(status.success(), "story terminal exit: {status:?}");
    let terminal_restoration = TerminalRestoration {
        mouse_capture_disabled: raw_contains(&raw, b"\x1b[?1000l"),
        sgr_mouse_disabled: raw_contains(&raw, b"\x1b[?1006l"),
        alternate_screen_left: raw_contains(&raw, b"\x1b[?1049l"),
    };
    assert!(
        terminal_restoration.mouse_capture_disabled
            && terminal_restoration.sgr_mouse_disabled
            && terminal_restoration.alternate_screen_left,
        "story terminal modes were not restored: {terminal_restoration:?}"
    );
    let trace = StoryTrace {
        schema: "plak.tui-trace.v1",
        story: story.name,
        status: "passed",
        steps: trace_steps,
        terminal_restoration,
    };
    if let Some(parent) = trace_path.parent() {
        std::fs::create_dir_all(parent).expect("trace parent");
    }
    std::fs::write(
        &trace_path,
        serde_json::to_vec_pretty(&trace).expect("trace JSON"),
    )
    .expect("trace written");
    std::fs::set_permissions(&trace_path, std::fs::Permissions::from_mode(0o600))
        .expect("trace mode");
}

// ---------------------------------------------------------------------------
// Session carry-forward
// ---------------------------------------------------------------------------

/// Exports `session` to `<fixture>/<name>`, checks the private file it wrote
/// and returns the document's path and contents.
fn export_session(fixture: &Fixture, session: &str, name: &str) -> (String, Value) {
    let path = fixture.root.join(name);
    let path_text = path.to_str().expect("utf-8 path").to_owned();
    let exported = run_cli(
        fixture,
        &["session", "export", session, "--out", &path_text],
    );
    assert!(exported.status.success(), "{}", exported.stderr);
    assert_eq!(exported.stdout.trim(), path_text);
    let mode = std::fs::metadata(&path)
        .expect("export file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let document: Value =
        serde_json::from_slice(&std::fs::read(&path).expect("export document")).expect("json");
    assert_eq!(document["format"], "plak-session-export");
    assert_eq!(document["version"], 7);
    assert!(document["source_resolved_count"].is_u64());
    assert!(
        document["pending_work"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["kind"] != "source_resolved_count")
    );
    assert_eq!(document["source"]["session_id"], session);
    assert_eq!(document["source"]["producer"]["status"], "verified");
    assert_eq!(
        document["source"]["producer"]["binary_id"]
            .as_str()
            .expect("producer binary id")
            .len(),
        12
    );
    (path_text, document)
}

/// A session exported to the portable document comes back through
/// `session import` as a new session that names where it came from, lists the
/// link, and is immediately promptable from a bounded checkpoint while the
/// full archive remains explicit reference data.
#[test]
fn an_exported_session_imports_as_a_linked_session_that_carries_on() {
    let fixture = Fixture::new(&two_tool_round_trips("first answer"));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "first prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let old = first.session_id();

    let (path_text, document) = export_session(&fixture, &old, "carried.json");
    let carried = document["checkpoint"].to_string();
    assert!(carried.contains("first prompt"), "{carried}");
    assert!(carried.contains("first answer"), "{carried}");
    let archive = document["archive"].to_string();
    assert!(archive.contains("first answer"), "{archive}");

    let imported = run_cli(&fixture, &["session", "import", &path_text]);
    assert!(imported.status.success(), "{}", imported.stderr);
    assert!(
        imported.stdout.contains(&format!("linked from {old} (")),
        "{}",
        imported.stdout
    );
    let new = imported
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plak --resume "))
        .expect("new session id")
        .to_owned();
    assert_ne!(new, old);

    let listed = run_cli(&fixture, &["session", "list"]);
    assert!(listed.status.success(), "{}", listed.stderr);
    assert!(
        listed.stdout.contains("carried forward from"),
        "{}",
        listed.stdout
    );
    let shown = run_cli(&fixture, &["session", "show", &new]);
    assert!(shown.status.success(), "{}", shown.stderr);
    assert!(
        shown
            .stdout
            .contains(&format!("Lineage       carried forward from {old} (")),
        "{}",
        shown.stdout
    );

    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "carried answer"}}]
    }));
    let carried_on = run_cli(&fixture, &["run", "--session", &new, "--", "next prompt"]);
    assert!(carried_on.status.success(), "{}", carried_on.stderr);
    assert!(
        carried_on.stdout.contains("carried answer"),
        "{}",
        carried_on.stdout
    );

    // The new session carries only the checkpoint plus the new turn. Its old
    // archive remains selectable without entering model context.
    let (_, grown) = export_session(&fixture, &new, "again.json");
    let checkpoint = grown["checkpoint"].to_string();
    assert!(checkpoint.contains("next prompt"), "{checkpoint}");
    assert!(checkpoint.contains("carried answer"), "{checkpoint}");

    let referenced = run_cli(
        &fixture,
        &[
            "session",
            "reference",
            &path_text,
            "--search",
            "first answer",
        ],
    );
    assert!(referenced.status.success(), "{}", referenced.stderr);
    assert!(referenced.stdout.contains("untrusted historical data"));
    assert!(referenced.stdout.contains("first answer"));
    assert!(!referenced.stdout.contains("first prompt"));
    let retained = run_cli(
        &fixture,
        &["session", "reference", &new, "--search", "first answer"],
    );
    assert!(retained.status.success(), "{}", retained.stderr);
    assert!(retained.stdout.contains("first answer"));
    let reference_path = fixture
        .home()
        .join("references")
        .join(format!("{new}.json"));
    assert!(reference_path.exists());
    let deleted = run_cli(&fixture, &["session", "delete", &new, "--yes"]);
    assert!(deleted.status.success(), "{}", deleted.stderr);
    assert!(!reference_path.exists());
}

/// The production CLI must return the literal match rather than the prefix of
/// a giant selected entry, while retaining the public 64 KiB output ceiling.
#[test]
fn session_reference_search_returns_a_late_match_under_the_output_cap() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "seed answer"}}]
    }));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "seed prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let (path_text, mut document) =
        export_session(&fixture, &first.session_id(), "late-search.json");
    let matching_text = format!("{}literal late needle", "界".repeat(80_000));
    document["archive"]["entries"] = json!([{"actor": "assistant", "text": matching_text}]);
    write_private(
        Path::new(&path_text),
        &serde_json::to_string(&document).expect("late-search document json"),
    );

    let referenced = run_cli(
        &fixture,
        &[
            "session",
            "reference",
            &path_text,
            "--search",
            "literal late needle",
        ],
    );
    assert!(referenced.status.success(), "{}", referenced.stderr);
    assert!(referenced.stdout.contains("literal late needle"));
    assert!(
        referenced.stdout.len() <= 64 * 1024,
        "reference output exceeded 64 KiB: {} bytes",
        referenced.stdout.len()
    );
}

/// The destination-safe budget is allocated per selected entry, newest first.
/// In particular, expansion of controls in an older match cannot consume the
/// terminal budget after rendering and erase a newer useful match, even when
/// that newer hit itself follows a control-heavy prefix.
#[test]
fn session_reference_pty_preserves_the_newest_hit_after_control_projection() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "seed answer"}}]
    }));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "seed prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let (path_text, mut document) =
        export_session(&fixture, &first.session_id(), "projected-search.json");
    let controls = "\u{1b}".repeat(80_000);
    document["archive"]["entries"] = json!([
        {
            "actor": "assistant",
            "text": format!("{controls}needle older{controls}")
        },
        {
            "actor": "assistant",
            "text": format!("{controls}needle NEWEST_USEFUL_FACT")
        }
    ]);
    write_private(
        Path::new(&path_text),
        &serde_json::to_string(&document).expect("projected-search document json"),
    );

    let arguments = [
        "session",
        "reference",
        &path_text,
        "--search",
        "needle",
        "--tail",
        "2",
    ];
    let piped = run_cli(&fixture, &arguments);
    assert!(piped.status.success(), "{}", piped.stderr);
    assert!(piped.stdout.contains("needle NEWEST_USEFUL_FACT"));
    assert!(piped.stdout.as_bytes().contains(&0x1b));
    assert!(piped.stdout.len() <= 64 * 1024);

    let (status, terminal_bytes) = run_cli_pty(&fixture, &arguments);
    assert!(status.success());
    assert!(!terminal_bytes.contains(&0x1b));
    let terminal = String::from_utf8(terminal_bytes).expect("terminal UTF-8");
    assert!(terminal.contains("needle NEWEST_USEFUL_FACT"), "{terminal}");
    let normalized = terminal.replace("\r\n", "\n");
    assert!(
        normalized.len() <= 64 * 1024,
        "terminal projection exceeded 64 KiB after CRLF normalization: {} bytes",
        normalized.len()
    );
}

/// A session whose turn never finished is refused with its cause, and no
/// document is written.
#[test]
fn an_unfinished_turn_refuses_export_and_writes_nothing() {
    let fixture = Fixture::new(&stalled_answer());
    let mut child = spawn_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "stall please",
        ],
    );
    wait_for_provider_dispatch(&fixture);
    signal(child.id(), SIGKILL);
    let killed = wait_with_output(&mut child);
    let session = killed
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("session_id="))
        .expect("the session id was printed before the kill")
        .to_owned();

    let path = fixture.root.join("refused.json");
    let path_text = path.to_str().expect("utf-8 path").to_owned();
    let refused = run_cli(
        &fixture,
        &["session", "export", &session, "--out", &path_text],
    );
    assert!(!refused.status.success());
    assert!(
        refused
            .stderr
            .contains(&format!("session {session} cannot be exported: ")),
        "{}",
        refused.stderr
    );
    assert!(!path.exists());
}

/// `plak session delete` asks first, then removes the session from listing,
/// inspection and resumption; a second delete finds nothing.
#[test]
fn a_deleted_session_is_gone_from_listing_and_inspection() {
    let fixture = Fixture::new(&two_tool_round_trips("doomed answer"));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "doomed prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let session = first.session_id();

    let unconfirmed = run_cli(&fixture, &["session", "delete", &session]);
    assert_eq!(unconfirmed.status.code(), Some(2));
    assert!(
        unconfirmed
            .stderr
            .contains("session delete requires interactive confirmation or explicit --yes"),
        "{}",
        unconfirmed.stderr
    );
    assert!(
        run_cli(&fixture, &["session", "show", &session])
            .status
            .success()
    );

    let deleted = run_cli(&fixture, &["session", "delete", &session, "--yes"]);
    assert!(deleted.status.success(), "{}", deleted.stderr);
    assert_eq!(deleted.stdout.trim(), format!("deleted session {session}"));
    assert!(
        !run_cli(&fixture, &["session", "show", &session])
            .status
            .success()
    );
    let listed = run_cli(&fixture, &["session", "list"]);
    assert!(listed.status.success(), "{}", listed.stderr);
    assert!(!listed.stdout.contains(&session[..8]), "{}", listed.stdout);
    assert!(
        !run_cli(&fixture, &["session", "delete", &session, "--yes"])
            .status
            .success()
    );
}

/// `plak session gc` reclaims a settled session's superseded checkpoints and
/// leaves it whole; a repeat reclaims nothing.
#[test]
fn session_gc_reclaims_superseded_history_and_keeps_the_session() {
    let fixture = Fixture::new(&two_tool_round_trips("kept answer"));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "keep this",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let session = first.session_id();

    let collected = run_cli(&fixture, &["session", "gc", &session]);
    assert!(collected.status.success(), "{}", collected.stderr);
    let reclaimed = collected
        .stdout
        .strip_prefix("reclaimed ")
        .and_then(|rest| rest.split_once(' '))
        .and_then(|(count, _)| count.parse::<u64>().ok())
        .expect("gc reports reclaimed checkpoints");
    assert!(reclaimed > 0, "{}", collected.stdout);
    assert!(
        collected
            .stdout
            .contains("(1 sessions pruned, 0 interrupted deletes finished)"),
        "{}",
        collected.stdout
    );
    let shown = run_cli(&fixture, &["session", "show", &session]);
    assert!(shown.status.success(), "{}", shown.stderr);

    let repeated = run_cli(&fixture, &["session", "gc", "--all"]);
    assert!(repeated.status.success(), "{}", repeated.stderr);
    assert!(
        repeated
            .stdout
            .starts_with("reclaimed 0 checkpoints, 0 objects, 0 bytes"),
        "{}",
        repeated.stdout
    );
}

/// An independent gc cannot acquire a live lease even at an exact Quiescent
/// head: this refusal is ownership, not the stale-head or Working-head guard.
#[test]
fn session_gc_skips_an_exact_quiescent_head_with_a_live_owner() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": "answered"}}]
    }));
    let session = answered_run(&fixture, "settled prompt");
    let mut owner = AgentProcess::start(&fixture, &["--session", &session]);
    owner.wait_for("ready", |record| record["kind"] == "ready");
    let collected = run_cli(&fixture, &["session", "gc", &session]);
    assert!(collected.status.success(), "{}", collected.stderr);
    assert!(
        collected
            .stdout
            .starts_with("reclaimed 0 checkpoints, 0 objects, 0 bytes"),
        "{}",
        collected.stdout
    );
    assert!(
        collected
            .stdout
            .contains(&format!("skipped {session}: it is open in a running plak")),
        "{}",
        collected.stdout
    );
    owner.send(&quit_frame());
    let outcome = owner.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
}

/// A session whose live owner holds its runtime lease is refused as open,
/// before anything is archived.
#[test]
fn an_owned_session_is_refused_deletion_as_open() {
    let fixture = Fixture::new(&stalled_answer());
    let mut child = spawn_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "stall please",
        ],
    );
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout pipe"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("session id line");
    let session = line
        .trim()
        .strip_prefix("session_id=")
        .expect("the session id is printed first")
        .to_owned();
    wait_for_provider_dispatch(&fixture);

    // The owner is alive and mid-turn, renewing its lease.
    let refused = run_cli(&fixture, &["session", "delete", &session, "--yes"]);
    let owner_alive = child.try_wait().expect("owner status").is_none();
    signal(child.id(), SIGKILL);
    let _ = child.wait();
    assert!(
        owner_alive,
        "the owner exited before the delete was refused"
    );
    assert!(!refused.status.success());
    assert!(
        refused.stderr.contains(&format!(
            "session {session} cannot be deleted: it is open in a running plak"
        )),
        "{}",
        refused.stderr
    );
    // gc applies the projector first, so it finds the killed turn's Working
    // head the feed names and leaves it for the recovery that closes it.
    let collected = run_cli(&fixture, &["session", "gc", "--all"]);
    assert!(collected.status.success(), "{}", collected.stderr);
    assert!(
        collected.stdout.contains(&format!(
            "skipped {session}: its turn is unfinished; run `plak session recover` first"
        )),
        "{}",
        collected.stdout
    );
    assert!(
        run_cli(&fixture, &["session", "show", &session])
            .status
            .success()
    );
}

/// Runtime sessions the store still retains, archived or not.
fn retained_runtime_sessions(fixture: &Fixture) -> i64 {
    let path = plak::home::paths::PlakPaths::explicit(fixture.home().clone()).runtime_database();
    let connection =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("runtime database opened read-only");
    connection
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .expect("runtime sessions counted")
}

/// The single runtime branch head in a store that retains exactly one
/// session, as a canonical locator.
fn only_runtime_locator(fixture: &Fixture) -> String {
    let path = plak::home::paths::PlakPaths::explicit(fixture.home().clone()).runtime_database();
    let connection =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("runtime database opened read-only");
    connection
        .query_row(
            "SELECT session_id, branch_id, checkpoint_id FROM branches",
            [],
            |row| {
                Ok(format!(
                    "{}/{}/{}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?
                ))
            },
        )
        .expect("exactly one runtime branch")
}

fn answered_run(fixture: &Fixture, prompt: &str) -> String {
    let run = run_cli(
        fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            prompt,
        ],
    );
    assert!(run.status.success(), "{}", run.stderr);
    run.session_id()
}

/// Deleting a session whose agent tree has a finished child archives the
/// session itself first, so while a running plak owns it the delete is
/// refused as open before the child's runtime session is touched. Once the
/// owner is gone the same delete removes the session, its finished child's
/// runtime session and the tree.
#[test]
fn an_open_root_is_refused_deletion_before_its_finished_child_is_touched() {
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::{NodeStatus, SpawnAgentRequest, WriterMode};

    let fixture = Fixture::new(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": "answered"}}]
    }));
    answered_run(&fixture, "the child's work");
    let child_locator = only_runtime_locator(&fixture);
    let root = answered_run(&fixture, "the root's work");
    let retained = retained_runtime_sessions(&fixture);
    assert_eq!(retained, 2);

    let mut owner = AgentProcess::start(&fixture, &["--session", &root]);
    owner.wait_for("ready", |record| record["kind"] == "ready");
    owner.send(&prompt_frame("hold the session open"));
    owner.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    });

    // The root's tree, with one finished child bound to the child session.
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home().clone());
    let checkout = tempfile::tempdir().expect("tree checkout");
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        root.clone(),
        "root",
        "owner",
        checkout.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree bootstrapped");
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("child spawned")
        .agent_id;
    tree.bind_session_locator(&child, child_locator)
        .expect("child session bound");
    tree.set_node_status_for_test(&child, NodeStatus::Completed);
    plak::store::tree::save_tree(&paths, &mut tree).expect("tree saved");

    let refused = run_cli(&fixture, &["session", "delete", &root, "--yes"]);
    assert!(!refused.status.success());
    assert!(
        refused.stderr.contains(&format!(
            "session {root} cannot be deleted: it is open in a running plak"
        )),
        "{}",
        refused.stderr
    );
    assert_eq!(
        retained_runtime_sessions(&fixture),
        retained,
        "a runtime session was deleted before the open refusal"
    );

    owner.send(&quit_frame());
    let outcome = owner.finish();
    assert!(outcome.status.success(), "{}", outcome.stderr);
    let deleted = run_cli(&fixture, &["session", "delete", &root, "--yes"]);
    assert!(deleted.status.success(), "{}", deleted.stderr);
    assert_eq!(
        retained_runtime_sessions(&fixture),
        0,
        "the session and its finished child were not both deleted"
    );
    assert!(!plak::store::tree::tree_exists(&paths, &root).expect("tree store read"));
}

/// A delete cut short right after it archived the session's own runtime
/// session is finished by `plak session gc --all` through the delete path:
/// the finished child's runtime session, the archived revision, the tree and
/// the product record all go, exactly as a rerun of `plak session delete`.
#[test]
fn gc_finishes_a_delete_cut_short_after_its_archive_through_the_delete_path() {
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::{NodeStatus, SpawnAgentRequest, WriterMode};

    let fixture = Fixture::new(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": "answered"}}]
    }));
    answered_run(&fixture, "the child's work");
    let child_locator = only_runtime_locator(&fixture);
    let root = answered_run(&fixture, "the root's work");
    assert_eq!(retained_runtime_sessions(&fixture), 2);

    let paths = plak::home::paths::PlakPaths::explicit(fixture.home().clone());
    let checkout = tempfile::tempdir().expect("tree checkout");
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        root.clone(),
        "root",
        "owner",
        checkout.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree bootstrapped");
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("child spawned")
        .agent_id;
    tree.bind_session_locator(&child, child_locator.clone())
        .expect("child session bound");
    tree.set_node_status_for_test(&child, NodeStatus::Completed);
    plak::store::tree::save_tree(&paths, &mut tree).expect("tree saved");

    // The delete's first step, then the crash: the root's runtime session is
    // archived and nothing else has changed.
    let root_locator = {
        let connection = rusqlite::Connection::open_with_flags(
            paths.runtime_database(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("runtime database opened read-only");
        connection
            .query_row(
                "SELECT session_id || '/' || branch_id || '/' || checkpoint_id FROM branches \
                 WHERE session_id || '/' || branch_id || '/' || checkpoint_id != ?1",
                [&child_locator],
                |row| row.get::<_, String>(0),
            )
            .expect("the root's runtime branch")
    };
    let sessions = plakous::sessions::AgentSessions::new(
        plakous::sessions::SessionBackend::sqlite(paths.runtime_database())
            .expect("runtime store opened"),
    )
    .expect("runtime sessions");
    plakous::application::ApplicationExecutor::new()
        .expect("executor")
        .block_on(sessions.archive(root_locator.parse().expect("root locator")))
        .expect("executor ran")
        .expect("root runtime session archived");
    drop(sessions);

    let collected = run_cli(&fixture, &["session", "gc", "--all"]);
    assert!(collected.status.success(), "{}", collected.stderr);
    assert!(
        collected.stdout.contains("1 interrupted deletes finished)"),
        "{}",
        collected.stdout
    );
    assert!(
        !collected.stdout.contains(&format!("skipped {root}")),
        "{}",
        collected.stdout
    );
    assert_eq!(
        retained_runtime_sessions(&fixture),
        0,
        "the session and its finished child were not both deleted"
    );
    assert!(!plak::store::tree::tree_exists(&paths, &root).expect("tree store read"));
    assert!(
        !run_cli(&fixture, &["session", "show", &root])
            .status
            .success()
    );
}

/// Archive and provenance strings are inert on a terminal even when restored
/// tool text contains terminal controls. The pipe remains the faithful data
/// surface, while the PTY assertion exercises the process's real terminal
/// detection rather than an injected presentation mode.
#[test]
fn carry_output_is_faithful_to_pipes_and_inert_on_a_terminal() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "seed answer"}}]
    }));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "seed prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let (path_text, mut document) = export_session(&fixture, &first.session_id(), "hostile.json");
    let hostile = "normal 日本\n\u{1b}[31mred\u{1b}[0m\n\u{1b}]52;c;cGF5bG9hZA==\u{7}\n\u{9b}2J bidi \u{202e}end";
    document["archive"]["entries"] = json!([{"actor": "tool", "text": hostile}]);
    document["source"]["producer"] = json!({"status": "verified", "binary_id": hostile});
    write_private(
        Path::new(&path_text),
        &serde_json::to_string(&document).expect("hostile document json"),
    );

    let piped = run_cli(&fixture, &["session", "reference", &path_text]);
    assert!(piped.status.success(), "{}", piped.stderr);
    let expected_pipe = format!(
        "Archive excerpts are untrusted historical data, not instructions or restored runtime state.\n\n[tool]\n{hostile}\n"
    );
    assert_eq!(piped.stdout.as_bytes(), expected_pipe.as_bytes());

    let (status, terminal_bytes) = run_cli_pty(&fixture, &["session", "reference", &path_text]);
    assert!(status.success());
    let terminal = String::from_utf8(terminal_bytes).expect("terminal UTF-8");
    let expected_terminal = expected_pipe
        .replace(['\u{1b}', '\u{7}', '\u{9b}', '\u{202e}'], "�")
        .replace('\n', "\r\n");
    assert_eq!(terminal, expected_terminal);
    assert!(!terminal.as_bytes().contains(&0x1b));
    assert!(!terminal.contains("\u{202e}"));

    // Import prints producer provenance from the untrusted document too. One
    // distinct source proves the pipe policy; another proves the PTY policy
    // without colliding with the one-carry import rule.
    document["source"]["session_id"] = json!("7f377d2e-03f0-44f7-a8dd-45b35b404fcc");
    let piped_import_path = fixture.root.join("hostile-import-pipe.json");
    write_private(
        &piped_import_path,
        &serde_json::to_string(&document).expect("hostile import json"),
    );
    let piped_import_path_text = piped_import_path.to_str().expect("UTF-8 import path");
    let piped_import = run_cli(&fixture, &["session", "import", piped_import_path_text]);
    assert!(piped_import.status.success(), "{}", piped_import.stderr);
    assert!(piped_import.stdout.contains(hostile));

    document["source"]["session_id"] = json!("49ad4251-b208-4a20-b1e2-c960153776a6");
    let import_path = fixture.root.join("hostile-import-terminal.json");
    write_private(
        &import_path,
        &serde_json::to_string(&document).expect("hostile terminal import json"),
    );
    let import_path_text = import_path.to_str().expect("UTF-8 import path");
    let (status, terminal_bytes) = run_cli_pty(&fixture, &["session", "import", import_path_text]);
    assert!(status.success());
    let terminal = String::from_utf8(terminal_bytes).expect("import terminal UTF-8");
    assert!(terminal.contains("normal 日本"), "{terminal:?}");
    assert!(terminal.contains("�[31mred�[0m"), "{terminal:?}");
    assert!(terminal.contains("�]52;c;cGF5bG9hZA==�"), "{terminal:?}");
    assert!(!terminal.as_bytes().contains(&0x1b));
    assert!(!terminal.contains("\u{202e}"));
}

/// One session is carried forward once: importing the same document again is
/// refused and names the session it already became.
#[test]
fn a_second_import_of_the_same_export_is_refused_by_name() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": {"text": "only answer"}}]
    }));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "first prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let (path, _) = export_session(&fixture, &first.session_id(), "once.json");
    let imported = run_cli(&fixture, &["session", "import", &path]);
    assert!(imported.status.success(), "{}", imported.stderr);
    let new = imported
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plak --resume "))
        .expect("new session id")
        .to_owned();
    let again = run_cli(&fixture, &["session", "import", &path]);
    assert!(!again.status.success());
    assert!(
        again
            .stderr
            .contains(&format!("was already carried forward as {new}")),
        "{}",
        again.stderr
    );
}

/// A carry file the new build cannot import is kept, and the failure names
/// where it is.
#[test]
fn a_failed_import_resume_keeps_the_carry_file_and_names_it() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let path = fixture.root.join("future.json");
    std::fs::write(&path, r#"{"format":"plak-session-export","version":99}"#).expect("document");
    let path_text = path.to_str().expect("utf-8 path").to_owned();
    let failed = run_cli(&fixture, &["--import-resume", &path_text]);
    assert!(!failed.status.success());
    assert!(
        failed
            .stderr
            .contains(&format!("the export is kept at {path_text}")),
        "{}",
        failed.stderr
    );
    assert!(failed.stderr.contains("version 99"), "{}", failed.stderr);
    assert!(path.exists());
}

#[test]
fn import_refuses_invalid_config_before_creating_a_successor() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": [{"output": {"text": "answer"}}]}));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--print-session-id",
            "--",
            "prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let (path, _) = export_session(&fixture, &first.session_id(), "strict-import.json");
    let before = retained_runtime_sessions(&fixture);
    std::fs::write(
        fixture.home().join("config.toml"),
        "[tui]\ncollapsed_edit_blocks = true\n",
    )
    .unwrap();
    let imported = run_cli(&fixture, &["session", "import", &path]);
    assert!(!imported.status.success(), "{}", imported.stdout);
    assert!(
        imported.stderr.contains("collapsed_edit_blocks"),
        "{}",
        imported.stderr
    );
    assert_eq!(retained_runtime_sessions(&fixture), before);
}

#[test]
fn nested_workspace_import_refuses_broken_trusted_parent_before_successor() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let project = fixture.workspace().join(".plak");
    std::fs::create_dir_all(&project).unwrap();
    write_private(
        &project.join("config.toml"),
        "[tui]\ncollapsed_edit_blocks = true\n",
    );
    assert!(run_cli(&fixture, &["config", "trust"]).status.success());
    let nested = fixture.workspace().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let mut document: Value =
        serde_json::from_str(include_str!("../fixtures/session-export-v6.json")).unwrap();
    document["workspace_root"] = json!(nested);
    let path = fixture.root.join("nested-import.json");
    write_private(&path, &serde_json::to_string(&document).unwrap());
    for args in [
        vec!["session", "import", path.to_str().unwrap()],
        vec!["--import-resume", path.to_str().unwrap()],
    ] {
        let mut child = fixture
            .command()
            .current_dir(&nested)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let refused = wait_with_output(&mut child);
        assert!(!refused.status.success(), "{}", refused.stdout);
        assert!(
            refused.stderr.contains("collapsed_edit_blocks"),
            "{}",
            refused.stderr
        );
        let config_path = project.join("config.toml");
        assert_eq!(
            refused
                .stderr
                .matches(config_path.to_str().unwrap())
                .count(),
            1,
            "{}",
            refused.stderr
        );
        assert!(path.is_file());
        assert!(
            !plak::home::paths::PlakPaths::explicit(fixture.home())
                .runtime_database()
                .exists()
        );
    }
}

#[test]
fn home_workspace_import_does_not_treat_product_home_as_overlay() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let home = fixture.root.join(".plak");
    std::fs::rename(fixture.home(), &home).unwrap();
    let mut document: Value =
        serde_json::from_str(include_str!("../fixtures/session-export-v6.json")).unwrap();
    document["workspace_root"] = json!(fixture.root);
    let path = fixture.root.join("home-import.json");
    write_private(&path, &serde_json::to_string(&document).unwrap());
    let mut child = fixture
        .command()
        .env("HOME", &fixture.root)
        .env("PLAK_HOME", &home)
        .current_dir(&fixture.root)
        .args(["session", "import", path.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let imported = wait_with_output(&mut child);
    assert!(imported.status.success(), "{}", imported.stderr);
    assert!(
        !imported.stderr.contains("not trusted"),
        "{}",
        imported.stderr
    );
}

fn fake_foreign_snapshot(fixture: &Fixture, version: &str) -> (String, PathBuf) {
    let key = "r999.e999.p999.t999";
    let id = "5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11".to_owned();
    let foreign = fixture.home().join("state").join(key);
    std::fs::create_dir_all(&foreign).unwrap();
    write_private(&foreign.join(format!("{id}.events.jsonl")), "");
    let snapshot = fixture
        .root
        .join("fake-home/.local/lib/plak/maintainer")
        .join("a".repeat(64));
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o700)).unwrap();
    write_private(&snapshot.join("state-key"), key);
    write_private(&snapshot.join("export-version"), version);
    let marker = fixture.root.join("snapshot-ran");
    write_private(
        &snapshot.join("plak"),
        &format!(
            "#!/bin/sh\ntouch '{}'\necho export-was-run >&2\nexit 1\n",
            marker.display()
        ),
    );
    std::fs::set_permissions(
        snapshot.join("plak"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    (id, marker)
}

#[test]
fn foreign_resume_skips_unimportable_snapshot_without_creating_carry_file() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let (id, marker) = fake_foreign_snapshot(&fixture, "999\n");
    let (status, output) = run_cli_pty(&fixture, &["--resume", &id]);
    let refusal = String::from_utf8_lossy(&output);
    assert!(!status.success());
    assert!(refusal.contains("version 999"), "{}", refusal);
    assert!(refusal.contains("newest carried session"), "{}", refusal);
    assert!(refusal.contains("plak session list"), "{}", refusal);
    assert!(!marker.exists());
    assert!(!fixture.home().join("carry").exists());
}

#[test]
fn foreign_resume_checks_config_before_snapshot_export() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let (id, marker) = fake_foreign_snapshot(&fixture, "5\n");
    write_private(
        &fixture.home().join("config.toml"),
        "[tui]\ncollapsed_edit_blocks = true\n",
    );
    let (status, output) = run_cli_pty(&fixture, &["--resume", &id]);
    let refusal = String::from_utf8_lossy(&output);
    assert!(!status.success());
    assert!(refusal.contains("collapsed_edit_blocks"), "{}", refusal);
    assert!(refusal.contains("retry the same command"), "{}", refusal);
    assert!(!refusal.contains("newest carried session"), "{}", refusal);
    assert!(!marker.exists());
    assert!(!fixture.home().join("carry").exists());
}

#[test]
fn foreign_resume_unparseable_manifest_uses_export_fallback_without_version_remedy() {
    for version in ["", "not a version"] {
        let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
        let (id, marker) = fake_foreign_snapshot(&fixture, version);
        let (status, output) = run_cli_pty(&fixture, &["--resume", &id]);
        let refusal = String::from_utf8_lossy(&output);
        assert!(!status.success());
        assert!(marker.is_file());
        assert!(refusal.contains("export-was-run"), "{}", refusal);
        assert!(!refusal.contains("newest carried session"), "{}", refusal);
        assert_eq!(
            std::fs::read_dir(fixture.home().join("carry"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn import_resume_requires_a_terminal_before_creating_a_successor() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": [{"output": {"text": "answer"}}]}));
    let first = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--print-session-id",
            "--",
            "prompt",
        ],
    );
    assert!(first.status.success(), "{}", first.stderr);
    let (path, _) = export_session(&fixture, &first.session_id(), "tty-import.json");
    let before = retained_runtime_sessions(&fixture);
    let imported = run_cli(&fixture, &["--import-resume", &path]);
    assert!(!imported.status.success());
    assert!(
        imported.stderr.contains("needs a terminal"),
        "{}",
        imported.stderr
    );
    assert_eq!(retained_runtime_sessions(&fixture), before);
    assert!(Path::new(&path).is_file());
}

#[test]
fn reload_probe_reports_strict_effective_config_without_creating_sessions() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let version = run_cli(&fixture, &["version", "--export-version"]);
    assert!(version.status.success(), "{}", version.stderr);
    assert_eq!(version.stdout.trim(), "7");
    let valid = run_cli(&fixture, &["version", "--reload-probe"]);
    assert!(valid.status.success(), "{}", valid.stderr);
    let verdict: Value = serde_json::from_str(&valid.stdout).unwrap();
    assert_eq!(verdict["config_error"], Value::Null);
    assert_eq!(verdict["export_versions"], json!([6, 7]));

    let project = fixture.workspace().join(".plak");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("config.toml"),
        "[tui]\ncollapsed_edit_blocks = true\n",
    )
    .unwrap();
    assert!(run_cli(&fixture, &["config", "trust"]).status.success());
    let invalid = run_cli(&fixture, &["version", "--reload-probe"]);
    assert!(invalid.status.success(), "{}", invalid.stderr);
    let verdict: Value = serde_json::from_str(&invalid.stdout).unwrap();
    let error = verdict["config_error"].as_str().unwrap();
    assert!(error.contains("collapsed_edit_blocks"), "{error}");
    assert!(
        error.contains(project.join("config.toml").to_str().unwrap()),
        "{error}"
    );
    assert!(
        !plak::home::paths::PlakPaths::explicit(fixture.home())
            .runtime_database()
            .exists()
    );
}

#[test]
fn doctor_checks_trusted_project_config_and_template_files() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let project = fixture.workspace().join(".plak");
    std::fs::create_dir_all(project.join("agents")).unwrap();
    std::fs::write(
        project.join("config.toml"),
        "[tui]\ncollapsed_edit_blocks = true\n",
    )
    .unwrap();
    assert!(run_cli(&fixture, &["config", "trust"]).status.success());
    let report = run_cli(&fixture, &["doctor"]);
    assert!(!report.status.success(), "{}", report.stderr);
    assert!(
        report.stdout.contains("collapsed_edit_blocks"),
        "{}",
        report.stdout
    );
    assert!(
        report.stdout.contains("plak config edit"),
        "{}",
        report.stdout
    );
    std::fs::write(project.join("config.toml"), "[tui]\nmouse = true\n").unwrap();
    std::fs::write(project.join("agents/broken.toml"), "model =\n").unwrap();
    let report = run_cli(&fixture, &["doctor"]);
    assert!(!report.status.success(), "{}", report.stderr);
    assert!(report.stdout.contains("broken.toml"), "{}", report.stdout);
}

#[test]
fn a_refused_import_seed_does_not_publish_a_retained_reference() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let document: Value = serde_json::from_str(include_str!("../fixtures/session-export-v6.json"))
        .expect("sanitized previous export");
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    std::fs::create_dir_all(paths.runtime_database()).expect("block the seed's runtime store open");
    let path = fixture.root.join("refused-seed.json");
    write_private(&path, &serde_json::to_string(&document).expect("document"));
    let refused = run_cli(
        &fixture,
        &["session", "import", path.to_str().expect("UTF-8")],
    );
    assert!(!refused.status.success());
    assert!(!fixture.home().join("references").exists());
}

#[test]
fn retained_reference_failure_warns_without_failing_a_committed_import() {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let mut document: Value =
        serde_json::from_str(include_str!("../fixtures/session-export-v6.json"))
            .expect("sanitized previous export");
    document["workspace_root"] = json!(fixture.workspace());
    let path = fixture.root.join("import-without-reference.json");
    write_private(&path, &serde_json::to_string(&document).expect("document"));
    std::fs::create_dir_all(fixture.home()).expect("home");
    std::fs::write(fixture.home().join("references"), b"not a directory")
        .expect("block retained reference directory");
    let imported = run_cli(
        &fixture,
        &["session", "import", path.to_str().expect("UTF-8")],
    );
    assert!(imported.status.success(), "{}", imported.stderr);
    let new = imported
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plak --resume "))
        .expect("created session id");
    assert!(
        imported.stdout.contains(&format!(
            "session {new} was created without its retained reference"
        )),
        "{}",
        imported.stdout
    );
    let shown = run_cli(&fixture, &["session", "show", new]);
    assert!(shown.status.success(), "{}", shown.stderr);
    assert!(path.exists());
    let deleted = run_cli(&fixture, &["session", "delete", new, "--yes"]);
    assert!(deleted.status.success(), "{}", deleted.stderr);
    let listed = run_cli(&fixture, &["session", "list"]);
    assert!(listed.status.success(), "{}", listed.stderr);
    assert!(!listed.stdout.contains(new), "{}", listed.stdout);
}

// ---------------------------------------------------------------------------
// Durability chains
//
// One session, several gestures. Separate tests that each do one gesture
// stay green while a real session dies; these fail unless the gestures
// land in order on one spawned `plak`.
// ---------------------------------------------------------------------------

fn child_inspection_fixture() -> (Fixture, String, String, String, String) {
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::{NodeStatus, SpawnAgentRequest, WriterMode};

    let fixture = Fixture::new(&json!({
        "version": 1, "repeat_last_step": true,
        "steps": [{"output": {"text": "child-only answer"}}]
    }));
    let child_session = answered_run(&fixture, "child-only prompt");
    let child_locator = only_runtime_locator(&fixture);
    let continued = run_cli(
        &fixture,
        &[
            "run",
            "--session",
            &child_session,
            "--",
            "child-only follow-up",
        ],
    );
    assert!(continued.status.success(), "{}", continued.stderr);
    fixture.write_script(&json!({
        "version": 1, "repeat_last_step": true,
        "steps": [{"output": {"text": "root-only answer"}}]
    }));
    let root = answered_run(&fixture, "root-only prompt");
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        root.clone(),
        "root",
        "owner",
        fixture.workspace(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("child")
        .agent_id;
    tree.bind_session_locator(&child, child_locator.clone())
        .expect("locator");
    tree.set_node_status_for_test(&child, NodeStatus::Completed);
    let unbound = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("unbound child")
        .agent_id;
    plak::store::tree::save_tree(&paths, &mut tree).expect("saved tree");

    (fixture, root, child, unbound, child_locator)
}

fn read_only_home_snapshot(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    use sha2::{Digest as _, Sha256};
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path).expect("home directory") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            files.extend(read_only_home_snapshot(&path));
        } else if !path.to_string_lossy().ends_with("-shm") {
            let bytes = std::fs::read(&path).expect("durable file");
            // SQLite readers may create empty WAL/SHM coordination files.
            if path.to_string_lossy().ends_with("-wal") && bytes.is_empty() {
                continue;
            }
            files.push((path, Sha256::digest(bytes).to_vec()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[test]
fn session_child_show_and_log_read_only_use_the_child_locator() {
    let (fixture, root, child, unbound, child_locator) = child_inspection_fixture();
    let before = read_only_home_snapshot(&fixture.home());
    let address = format!("{root}.{child}");
    let show = run_cli(&fixture, &["session", "show", &address]);
    assert!(show.status.success(), "{}", show.stderr);
    assert!(show.stdout.contains(&address), "{}", show.stdout);
    assert!(show.stdout.contains("Completed") && show.stdout.contains(&child_locator));
    assert!(show.stdout.contains("quiescent"));
    assert!(show.stdout.contains("recorded locator is stale; unchanged"));
    let log = run_cli(&fixture, &["session", "log", &address, "--json"]);
    assert!(log.status.success(), "{}", log.stderr);
    assert!(log.stdout.contains("child-only answer"), "{}", log.stdout);
    assert!(!log.stdout.contains("root-only"), "{}", log.stdout);
    for line in log.stdout.lines() {
        serde_json::from_str::<Value>(line).expect("child log JSON record");
    }
    let tail = run_cli(
        &fixture,
        &["session", "log", &address, "--tail", "1", "--json"],
    );
    assert!(tail.status.success(), "{}", tail.stderr);
    assert_eq!(tail.stdout.lines().count(), 1);
    let filtered = run_cli(
        &fixture,
        &["session", "log", &address, "--kind", "user", "--json"],
    );
    assert!(filtered.status.success(), "{}", filtered.stderr);
    assert!(filtered.stdout.contains("child-only prompt"));
    assert!(!filtered.stdout.contains("child-only answer"));
    let human = run_cli(&fixture, &["session", "log", &address]);
    assert!(human.status.success(), "{}", human.stderr);
    assert!(human.stdout.contains("child-only answer"));
    let no_locator = run_cli(&fixture, &["session", "log", &format!("{root}.{unbound}")]);
    assert!(!no_locator.status.success());
    assert!(
        no_locator.stderr.contains("locator unavailable"),
        "{}",
        no_locator.stderr
    );
    for command in ["show", "log"] {
        for suffix in ["c0", "c999"] {
            let unknown = run_cli(&fixture, &["session", command, &format!("{root}.{suffix}")]);
            assert!(!unknown.status.success());
            assert!(
                unknown.stderr.contains("unknown child"),
                "{}",
                unknown.stderr
            );
        }
        for suffix in ["c-1", "c01", "c", "x1", "c1.extra"] {
            assert!(
                !run_cli(&fixture, &["session", command, &format!("{root}.{suffix}")])
                    .status
                    .success()
            );
        }
    }
    assert_eq!(
        read_only_home_snapshot(&fixture.home()),
        before,
        "show/log wrote durable state"
    );
    let root_show = run_cli(&fixture, &["session", "show", &root]);
    assert!(root_show.stdout.contains(&format!("Session {root}")));
    assert!(
        root_show
            .stdout
            .contains("Canonical recovery (root session)")
    );
    assert!(
        root_show
            .stderr
            .contains("one or more child recovery heads could not be inspected")
    );
    let root_log = run_cli(&fixture, &["session", "log", &root, "--json"]);
    assert!(root_log.status.success(), "{}", root_log.stderr);
    assert!(root_log.stdout.contains("root-only answer"));
    assert!(
        !run_cli(&fixture, &["session", "delete", &address, "--yes"])
            .status
            .success()
    );
}

#[test]
fn non_inference_commands_create_no_wire_capture_run() {
    let fixture = Fixture::new(&json!({
        "version": 1, "steps": [{"output": {"text": "unused"}}]
    }));
    for args in [
        vec!["doctor"],
        vec!["session", "list"],
        vec!["state", "prune-legacy", "--yes"],
    ] {
        let output = run_cli(&fixture, &args);
        assert!(output.status.success(), "{args:?}: {}", output.stderr);
        assert!(!fixture.home().join("wire-captures").exists(), "{args:?}");
        assert!(!output.stderr.contains("private inference capture selected"));
    }
}

#[test]
fn session_show_distinguishes_live_working_owner_from_interrupted_head() {
    let fixture = Fixture::new(&json!({
        "version": 1, "repeat_last_step": true,
        "steps": [{"output": {"text": "committed answer"}}]
    }));
    let session = answered_run(&fixture, "committed prompt");
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "unfinished answer"}, "stall_ms": 60_000}]
    }));
    std::fs::write(fixture.root.join("evidence.jsonl"), b"").unwrap();
    let mut owner = AgentProcess::start(&fixture, &["--session", &session]);
    owner.wait_for("ready", |record| record["kind"] == "ready");
    owner.send(&prompt_frame("working prompt"));
    wait_for_provider_dispatch(&fixture);
    let live = run_cli(&fixture, &["session", "show", &session]);
    signal(owner.id(), SIGKILL);
    owner.close_stdin();
    assert!(!owner.finish().status.success());
    assert!(live.status.success(), "{}", live.stderr);
    assert!(
        live.stdout.contains("working (live owner present)"),
        "{}",
        live.stdout
    );
    assert!(live.stdout.contains("Phase"));
    assert!(live.stdout.contains("Obligation"));
    assert!(!live.stdout.contains("Status         recovery required"));
    let before = read_only_home_snapshot(&fixture.home());
    let abandoned = run_cli(&fixture, &["session", "show", &session]);
    assert!(abandoned.status.success(), "{}", abandoned.stderr);
    assert!(
        abandoned
            .stdout
            .contains("Status         recovery required"),
        "{}",
        abandoned.stdout
    );
    assert!(!abandoned.stdout.contains("live owner present"));
    assert!(abandoned.stdout.contains("Phase"));
    assert!(abandoned.stdout.contains("Obligation"));
    assert_eq!(read_only_home_snapshot(&fixture.home()), before);
}

#[test]
fn session_child_log_working_head_shows_only_committed_history_without_writes() {
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::{NodeStatus, SpawnAgentRequest, WriterMode};

    let fixture = Fixture::new(&json!({
        "version": 1, "repeat_last_step": true,
        "steps": [{"output": {"text": "committed child answer"}}]
    }));
    let child_session = answered_run(&fixture, "committed child prompt");
    let child_locator = only_runtime_locator(&fixture);
    fixture.write_script(&json!({
        "version": 1, "repeat_last_step": true,
        "steps": [{"output": {"text": "root-only answer"}}]
    }));
    let root = answered_run(&fixture, "root-only prompt");
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "uncommitted child answer"}, "stall_ms": 60_000}]
    }));
    std::fs::write(fixture.root.join("evidence.jsonl"), b"").expect("reset dispatch witness");
    let mut owner = AgentProcess::start(&fixture, &["--session", &child_session]);
    owner.wait_for("ready", |record| record["kind"] == "ready");
    owner.send(&prompt_frame("committed working prompt"));
    wait_for_provider_dispatch(&fixture);
    signal(owner.id(), SIGKILL);
    owner.close_stdin();
    assert!(!owner.finish().status.success());

    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        root.clone(),
        "root",
        "owner",
        fixture.workspace(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("child")
        .agent_id;
    tree.bind_session_locator(&child, child_locator)
        .expect("locator");
    tree.set_node_status_for_test(&child, NodeStatus::RecoveryRequired);
    plak::store::tree::save_tree(&paths, &mut tree).expect("saved tree");
    let before = read_only_home_snapshot(&fixture.home());
    let address = format!("{root}.{child}");
    let show = run_cli(&fixture, &["session", "show", &address]);
    assert!(show.status.success(), "{}", show.stderr);
    assert!(show.stdout.contains("recovery required"), "{}", show.stdout);
    let human = run_cli(&fixture, &["session", "log", &address]);
    assert!(human.status.success(), "{}", human.stderr);
    assert!(human.stdout.contains("Working head"), "{}", human.stdout);
    assert!(human.stdout.contains("phase="));
    assert!(
        human
            .stdout
            .contains("items after the last commit are not shown")
    );
    assert!(human.stdout.contains("committed child answer"));
    assert!(human.stdout.contains("committed working prompt"));
    assert!(!human.stdout.contains("uncommitted child answer"));
    assert!(!human.stdout.contains("root-only"));
    let json = run_cli(
        &fixture,
        &["session", "log", &address, "--tail", "1", "--json"],
    );
    assert!(json.status.success(), "{}", json.stderr);
    let lines: Vec<Value> = json
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON record"))
        .collect();
    assert_eq!(lines.len(), 2, "header plus one committed item");
    assert_eq!(lines[0]["kind"], "history_snapshot");
    assert_eq!(lines[0]["status"], "Working");
    assert!(lines[0]["recovery_phase"].is_string());
    assert!(
        lines[0]["note"]
            .as_str()
            .unwrap()
            .contains("items after the last commit")
    );
    assert_eq!(lines[1]["text"], "committed working prompt");
    assert_eq!(lines[1]["kind"], "user");
    assert_eq!(read_only_home_snapshot(&fixture.home()), before);
}

/// A committed fold is a `compaction` row. The summary ciphertext is not.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the fold, the child bind, and the log projection share one offline session"
)]
fn session_child_log_names_a_committed_compaction_and_hides_the_summary() {
    use plakous_hosted::agent_tree::ProductAgentTree;
    use plakous_hosted::agent_tree_protocol::{NodeStatus, SpawnAgentRequest, WriterMode};

    let fixture = Fixture::new(&json!({
        "version": 1,
        "maximum_projected_input": 20_000,
        "content_sensitive_projection": true,
        "fold": "accept",
        "steps": [
            {"output": {"text": "the first answer"}, "usage": {}},
            {"output": {"text": "the second answer"}, "usage": {}},
            {"output": {"text": "the compacted answer"}, "usage": {}}
        ]
    }));
    write_private(
        &fixture.home().join("config.toml"),
        "[root]\nagent = \"lead\"\n\n[orchestration]\nsubagents = false\n\n[compaction]\nenabled = false\n",
    );
    let mut agent = AgentProcess::start(&fixture, &[]);
    agent.wait_for("ready", |record| record["kind"] == "ready");
    agent.send(&prompt_frame("answer the first intent"));
    agent.wait_for("the first answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the first answer"
    });
    agent.wait_for_completed_turns(1);
    agent.send(&prompt_frame("answer the second intent"));
    agent.wait_for("the second answer", |record| {
        record["kind"] == "assistant" && record["text"] == "the second answer"
    });
    agent.wait_for_completed_turns(2);
    agent.send(&compact_frame());
    let compact = agent.wait_for("the compaction outcome", |record| {
        record["kind"] == "tool"
            && record["activity_kind"] == "compact"
            && matches!(record["state"].as_str(), Some("done" | "failed"))
    });
    assert_eq!(compact["state"], "done", "the fold committed: {compact}");
    agent.send(&quit_frame());
    let outcome = agent.finish();
    assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);

    let child_locator = only_runtime_locator(&fixture);
    fixture.write_script(&json!({
        "version": 1, "repeat_last_step": true,
        "steps": [{"output": {"text": "root-only answer"}}]
    }));
    let root = answered_run(&fixture, "root-only prompt");
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        root.clone(),
        "root",
        "owner",
        fixture.workspace(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("child")
        .agent_id;
    tree.bind_session_locator(&child, child_locator)
        .expect("locator");
    tree.set_node_status_for_test(&child, NodeStatus::Completed);
    plak::store::tree::save_tree(&paths, &mut tree).expect("saved tree");

    let address = format!("{root}.{child}");
    let json = run_cli(&fixture, &["session", "log", &address, "--json"]);
    assert!(json.status.success(), "{}", json.stderr);
    assert!(
        !json.stdout.contains("older intent groups compacted"),
        "the summary ciphertext leaked:\n{}",
        json.stdout
    );
    let lines: Vec<Value> = json
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON record"))
        .collect();
    let compactions: Vec<&Value> = lines
        .iter()
        .filter(|line| line["kind"] == "compaction")
        .collect();
    assert_eq!(compactions.len(), 1, "{lines:?}");
    let text = compactions[0]["text"].as_str().expect("compaction text");
    let Some(rest) = text.strip_prefix("compacted ") else {
        panic!("compaction text is not the projection phrase: {text}");
    };
    let (count, suffix) = rest.split_once(' ').expect(text);
    assert!(
        !count.is_empty() && count.chars().all(|byte| byte.is_ascii_digit()) && count != "0",
        "{text}"
    );
    assert_eq!(suffix, "earlier segments (semantic summary)", "{text}");
    assert_eq!(compactions[0]["tool_calls"], 0);
    assert_eq!(compactions[0]["tool_results"], 0);
    assert!(
        compactions[0]["entry_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(lines.iter().any(|line| line["kind"] == "user"));
    assert!(lines.iter().any(|line| line["kind"] == "assistant"));

    let only = run_cli(
        &fixture,
        &["session", "log", &address, "--kind", "compaction", "--json"],
    );
    assert!(only.status.success(), "{}", only.stderr);
    assert_eq!(only.stdout.lines().count(), 1);
    assert!(only.stdout.contains("semantic summary"));
    let assistants = run_cli(
        &fixture,
        &["session", "log", &address, "--kind", "assistant", "--json"],
    );
    assert!(assistants.status.success(), "{}", assistants.stderr);
    assert!(!assistants.stdout.contains("\"kind\":\"compaction\""));
    assert!(!assistants.stdout.contains("semantic summary"));
    let human = run_cli(&fixture, &["session", "log", &address]);
    assert!(human.status.success(), "{}", human.stderr);
    assert!(human.stdout.contains("compaction"), "{}", human.stdout);
    assert!(
        human.stdout.contains("semantic summary"),
        "{}",
        human.stdout
    );
    assert!(!human.stdout.contains("older intent groups compacted"));
}

mod durability {
    use super::*;

    use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};

    const CANCELLED_TEXT: &str = "must not land after cancel";
    const COMPACTED_ANSWER: &str = "the compacted answer";
    const RESUMED_ANSWER: &str = "resumed after the chain";
    const FAILURE_TEXT: &str = "scripted_child_failure";
    const ROOT_NAMES_FAILURE: &str = "the root names the failed child";
    const HONEST_NEXT: &str = "the next root prompt is honest";
    const PARTIAL_ANSWER: &str = "after the partial cell";

    /// Non-terminal or recovery node statuses `plak session show` prints.
    /// `Completed` and `Closed` are the quiet-success pair and are absent here.
    const RECOVERY_VISIBLE_NODES: [&str; 6] = [
        "node=Running",
        "node=Interrupted",
        "node=Sealing",
        "node=Paused",
        "node=Pending",
        "node=RecoveryRequired",
    ];

    fn spawn_arguments(description: &str) -> Value {
        json!({
            "template": "implement",
            "tier": "mutate",
            "workspace": "fork",
            "description": description,
            "prompt": "make the offline scripted change",
            "output_schema_json": "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
        })
    }

    fn tool_step(name: &str, arguments: &Value) -> Value {
        json!({
            "output": {"tool": {"name": name, "arguments": arguments}},
            "finish": "tool_calls"
        })
    }

    fn text_step(text: &str) -> Value {
        json!({"output": {"text": text}})
    }

    fn show_session(fixture: &Fixture, id: &str) -> CliRun {
        let mut child = fixture
            .command()
            .args(["session", "show", id])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("plak session show");
        wait_with_output(&mut child)
    }

    fn saved_session_id(fixture: &Fixture) -> String {
        let files = session_event_files(fixture);
        assert_eq!(files.len(), 1, "one session expected, found {files:?}");
        let name = files[0]
            .file_name()
            .and_then(|name| name.to_str())
            .expect("session event file name");
        name.strip_suffix(".events.jsonl")
            .expect("events file suffix")
            .to_owned()
    }

    fn session_events(fixture: &Fixture) -> String {
        session_events_for(fixture, &saved_session_id(fixture))
    }

    fn find_named(root: &Path, name: &str) -> Option<PathBuf> {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.file_name().and_then(|item| item.to_str()) == Some(name) {
                    return Some(path);
                }
                if path.is_dir() {
                    stack.push(path);
                }
            }
        }
        None
    }

    fn wait_for_named_file(root: &Path, name: &str) -> PathBuf {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(found) = find_named(root, name) {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "{name} never appeared under {}",
                root.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn evidence_rows(
        fixture: &Fixture,
    ) -> Vec<plakous_offline_crash_harness::evidence::EvidenceRecord> {
        read_evidence(fixture.root.join("evidence.jsonl")).unwrap_or_default()
    }

    /// Effect outcome, then a later provider dispatch: the cancel lands on the
    /// next model round, after the local effect has already committed.
    fn wait_for_effect_then_next_dispatch(fixture: &Fixture) {
        let deadline = Instant::now() + WAIT;
        loop {
            let rows = evidence_rows(fixture);
            let effect = rows
                .iter()
                .position(|row| row.kind == EvidenceKind::EffectOutcome);
            let later = effect.is_some_and(|index| {
                rows[index + 1..]
                    .iter()
                    .any(|row| row.kind == EvidenceKind::ProviderDispatched)
            });
            if later {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the completed effect never preceded the next provider dispatch: {rows:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn chain_script() -> Value {
        json!({
            "version": 1,
            "maximum_projected_input": 20_000,
            "scheduled_compaction": true,
            "content_sensitive_projection": true,
            "tool_result_bytes": 5_800,
            "fold": "accept",
            "steps": [
                tool_step("loopback_effect", &json!({})),
                {"output": {"text": CANCELLED_TEXT}, "stall_ms": 12_000},
                tool_step("loopback_effect", &json!({})),
                tool_step("loopback_effect", &json!({})),
                text_step(COMPACTED_ANSWER)
            ]
        })
    }

    /// A: tool, cancel after that effect, compact, quit, resume picker.
    ///
    /// The continuation script is rewritten before the picker process starts
    /// because a new process restarts the script cursor. The oracle is the
    /// persisted session id, the event log, and `plak session show`, not the
    /// script identity.
    #[test]
    fn durability_chain_tool_cancel_compact_quit_resume() {
        let fixture = Fixture::new(&chain_script());
        let mut terminal = TerminalChild::spawn(&fixture, 100, None);
        terminal.wait_screen("%/");
        terminal.send(b"do the tool then stall\r");
        // The stall round has been dispatched, and the tool effect before it
        // has an outcome. Esc is sent only once the face is in the cancellable
        // wait: a key that arrives while the composer is still idle does not
        // cancel, and the stall text would land.
        wait_for_effect_then_next_dispatch(&fixture);
        terminal.wait_where("the stall is cancellable", |frame| {
            assert!(
                !frame.contains(CANCELLED_TEXT),
                "the stall text landed before cancel:\n{frame}"
            );
            frame.contains("waiting for model")
        });
        terminal.send(b"\x1b");
        terminal.wait_screen("Stop this turn?");
        // Focus starts on Stop turn, so Enter cancels it.
        terminal.send(b"\r");
        let settled = terminal.wait_where("the cancelled turn settled", |frame| {
            assert!(
                !frame.contains(CANCELLED_TEXT),
                "the stall text landed after cancel:\n{frame}"
            );
            !frame.contains("cancelling")
                && !frame.contains("waiting for model")
                && !frame.contains("thinking")
                && frame.contains("⊘ interrupted")
        });
        assert!(
            !settled.contains(CANCELLED_TEXT),
            "the stall's text landed after cancel:\n{settled}"
        );

        terminal.send(b"compact after the cancel\r");
        // A noncommitted pressure fold may continue on a fitting retained head.
        // Wait for that turn to settle, then explicitly compact the completed
        // history: the chain still crosses a committed compaction before quit.
        terminal.wait_where("the fitting turn completed", |frame| {
            frame.contains(COMPACTED_ANSWER)
                && !frame.contains("thinking")
                && !frame.contains("waiting for model")
        });
        terminal.send(b"/compact\r");
        let compacted = terminal.wait_where("explicit compaction committed", |frame| {
            frame.contains(COMPACTED_ANSWER)
                && frame.contains("Compact")
                && (frame.contains('→') || frame.contains("->"))
                && frame.contains("⊘ interrupted")
                && !frame.contains("thinking")
                && !frame.contains("waiting for model")
        });
        assert!(
            !compacted.contains(CANCELLED_TEXT),
            "the cancelled text appeared on the compacting turn:\n{compacted}"
        );

        terminal.send(b"\x11");
        let (status, raw) = terminal.finish_with_raw();
        assert!(status.success(), "quit did not exit cleanly");
        assert!(
            raw.windows(b"\x1b[?1049l".len())
                .any(|window| window == b"\x1b[?1049l"),
            "quit must leave the alternate screen"
        );
        let hint = resume_hint_session(&raw).expect("resume hint");
        let session = session_id_for_hint(&fixture, &hint);
        let events = session_events_for(&fixture, &session);
        assert!(
            events.contains("tool.completed") && events.contains("loopback_effect"),
            "the completed local effect is missing from the event log:\n{events}"
        );

        resume_chain_through_picker(&fixture);

        let show = show_session(&fixture, &session);
        assert!(
            show.status.success(),
            "session show after the chain was not quiescent:\n{}\n{}",
            show.stdout,
            show.stderr
        );
        assert!(
            show.stdout.contains("quiescent"),
            "show did not report a quiescent root:\n{}",
            show.stdout
        );
        assert!(
            !show.stdout.contains("recovery required"),
            "the chained session was left recovery-required:\n{}",
            show.stdout
        );
        assert!(
            session_event_files(&fixture).iter().any(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&session))
            }),
            "the resumed chain session disappeared"
        );
    }

    /// Same fixture dials as the session that was saved. A new process
    /// restarts the script cursor, but the resolved harness plan has to
    /// match the recorded model or exact reopen is refused.
    ///
    /// No follow-up prompt. The session is already over the fence, and a
    /// further scheduled compact asks the scripted compaction binding to be
    /// an exact twin of the source, which is the held case in
    /// `an_explicit_compaction_whose_fold_is_accepted_commits_and_continues`.
    /// The restored transcript is the resume.
    fn resume_chain_through_picker(fixture: &Fixture) {
        fixture.write_script(&json!({
            "version": 1,
            "maximum_projected_input": 20_000,
            "scheduled_compaction": true,
            "content_sensitive_projection": true,
            "tool_result_bytes": 5_800,
            "fold": "accept",
            "steps": [{"output": {"text": RESUMED_ANSWER}}]
        }));
        let mut picker = TerminalChild::spawn(fixture, 100, Some(""));
        picker.wait_screen("Resume session");
        // The picker's `Enter` acts once it has been open for 600 ms.
        std::thread::sleep(Duration::from_millis(700));
        picker.send(b"\r");
        let restored = picker.wait_where("the picker reopened the chain", |frame| {
            !frame.contains("Resume session")
                && frame.contains(COMPACTED_ANSWER)
                && frame.contains("⊘ interrupted")
                && frame.contains("Compact")
                && frame.contains("/ commands")
        });
        assert!(
            !restored.contains(CANCELLED_TEXT),
            "resume painted the cancelled text:\n{restored}"
        );
        picker.send(b"\x11");
        assert!(picker.finish().success());
    }

    /// The session the quit hint named. A process can leave more than one
    /// event log; the hint is the session the product told the operator to
    /// reopen, and that is the chain.
    fn session_id_for_hint(fixture: &Fixture, hint: &str) -> String {
        let matches = session_event_files(fixture)
            .into_iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?.to_owned();
                let id = name.strip_suffix(".events.jsonl")?.to_owned();
                id.starts_with(hint).then_some(id)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matches.len(),
            1,
            "resume hint {hint} must name one session, found {matches:?}"
        );
        matches[0].clone()
    }

    fn session_events_for(fixture: &Fixture, id: &str) -> String {
        std::fs::read_to_string(
            plak::home::paths::PlakPaths::explicit(fixture.home()).session_events_file(id),
        )
        .expect("session events")
    }

    fn retiring_child_script() -> Value {
        json!({
            "version": 1,
            "tree": true,
            "durable_tree": true,
            "child_hold_after": 0,
            "child_hold_ms": 60_000,
            "steps": [
                tool_step("spawn_agent", &spawn_arguments("held child")),
                {"output": {"text": CANCELLED_TEXT}, "stall_ms": 8_000}
            ]
        })
    }

    /// B: Esc, then quit, while a child is still running after its locator
    /// publish. `plak session show` must name a non-terminal or recovery node.
    /// Exit 5 from an unparsable locator is not this assertion.
    #[test]
    fn durability_quit_during_child_retirement_is_recovery_visible() {
        let fixture = Fixture::new(&retiring_child_script());
        let mut terminal = TerminalChild::spawn(&fixture, 120, None);
        terminal.wait_screen("/10M");
        terminal.send(b"spawn and quit while it retires\r");
        wait_for_named_file(&fixture.root, "holding.txt");
        terminal.send(b"\x1b");
        terminal.send(b"\x11");
        let (status, raw) = terminal.finish_with_raw();
        assert!(
            status.success(),
            "quit during child retirement did not exit; cooked:\n{}",
            String::from_utf8_lossy(&raw)
        );
        assert!(
            !String::from_utf8_lossy(&raw).contains(CANCELLED_TEXT),
            "the stall text landed while the child was still retiring"
        );

        let session = saved_session_id(&fixture);
        let show = show_session(&fixture, &session);
        let visible = RECOVERY_VISIBLE_NODES
            .iter()
            .any(|status| show.stdout.contains(status));
        assert!(
            visible,
            "quit while a child was retiring was a quiet success. \
             An invalid locator alone is not recovery. show exit {:?} stdout:\n{}\nstderr:\n{}",
            show.status.code(),
            show.stdout,
            show.stderr
        );
        assert!(
            !show.stdout.contains("Children       none"),
            "the retiring child was not in the persisted tree:\n{}",
            show.stdout
        );
    }

    fn disk_full_script() -> Value {
        json!({
            "version": 1,
            "tree": true,
            "durable_tree": true,
            // The child row lands before disk_full at its bound locator save,
            // regardless of how many projector or prepare saves precede it.
            "disk_full_at_child_locator": true,
            "steps": [
                tool_step("spawn_agent", &spawn_arguments("publish during disk_full")),
                text_step("after the publish failure")
            ]
        })
    }

    /// C: classified `disk_full` during child publish. The session must not
    /// come back as a quiescent tree with no children.
    #[test]
    fn durability_disk_full_child_publish_stays_non_quiescent() {
        let fixture = Fixture::new(&disk_full_script());
        let mut agent = AgentProcess::start(&fixture, &[]);
        agent.wait_for("ready", |record| record["kind"] == "ready");
        agent.send(&prompt_frame("spawn into a full tree store"));
        agent.wait_for("the spawn settled", |record| {
            record["kind"] == "assistant_done"
                || (record["kind"] == "tool"
                    && record["state"] != "running"
                    && record.to_string().contains("disk_full"))
        });
        agent.send(&quit_frame());
        let outcome = agent.finish();
        let stream = format!("{}\n{}", outcome.lines.join("\n"), outcome.stderr);
        assert!(
            stream.contains("disk_full"),
            "the publish failure did not name disk_full:\n{stream}"
        );
        assert!(
            !stream.contains("SQLITE"),
            "disk_full leaked backend text:\n{stream}"
        );
        let session = saved_session_id(&fixture);
        assert!(
            plak::home::paths::PlakPaths::explicit(fixture.home())
                .session_events_file(&session)
                .is_file(),
            "the session directory was removed after disk_full"
        );
        let show = show_session(&fixture, &session);
        let quiet = show.status.success()
            && show.stdout.contains("quiescent")
            && (show.stdout.contains("Children       none") || !show.stdout.contains("Child"));
        assert!(
            !quiet,
            "disk_full during child publish left a quiescent session. \
             exit {:?} stdout:\n{}\nstderr:\n{}",
            show.status.code(),
            show.stdout,
            show.stderr
        );
        assert!(
            show.stdout.contains("Child")
                && (show.stdout.contains("locator unavailable")
                    || show.stdout.contains("recovery required")
                    || show.stdout.contains("node=RecoveryRequired")
                    || show.status.code() == Some(5)),
            "disk_full did not leave a recovery-visible child. exit {:?} stdout:\n{}\nstderr:\n{}",
            show.status.code(),
            show.stdout,
            show.stderr
        );
    }

    fn partial_cell_script() -> Value {
        code_mode_script(&json!([
            {"output": {"cell": "const one = await tools.exec_command({ cmd: \"sh -c 'printf nested-done > nested-done.txt'\" });\nawait tools.exec_command({ cmd: \"sh -c 'printf $$ > sleep.pid; exec sleep 31'\" });\n"}, "finish": "tool_calls"},
            text_step_in_code_mode()
        ]))
    }

    fn text_step_in_code_mode() -> Value {
        json!({"output": {"text": PARTIAL_ANSWER}})
    }

    /// Records at or after `from` only. A later prompt must not be satisfied
    /// by the previous turn's `assistant_done`.
    fn wait_for_new(
        agent: &AgentProcess,
        from: usize,
        what: &str,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let lines = agent.snapshot();
            for line in lines.iter().skip(from) {
                if let Ok(value) = serde_json::from_str::<Value>(line)
                    && predicate(&value)
                {
                    return value;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{what} never arrived within {}ms; stdout:\n{}\nstderr:\n{}",
                timeout.as_millis(),
                lines.join("\n"),
                agent.stderr.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// A host killed after an MCP call's write-ahead claim reopens with the
    /// call settled as outcome-unknown: the recovery note names the running
    /// tool, and the server never sees the call again.
    #[test]
    fn durability_mcp_call_killed_after_its_claim_is_never_redispatched() {
        let fixture = Fixture::new(&json!({
            "version": 1,
            "steps": [tool_step("mcp__fake__hang", &json!({}))]
        }));
        let log = fixture.root.join("mcp-calls.log");
        enable_fake_mcp(&fixture, &["--call-log", log.to_str().expect("log path")]);
        let mut agent = AgentProcess::start(&fixture, &[]);
        let hello = agent.wait_for("hello", |record| record["kind"] == "hello");
        let session = hello["session_id"].as_str().expect("session id").to_owned();
        agent.send(&prompt_frame("call the server"));
        wait_for_fixture_file(&log);
        signal(agent.id(), SIGKILL);
        agent.close_stdin();
        let _ = agent.finish();

        fixture.write_script(&recovery_answer_script());
        let mut reopened = reopen_until_the_lease_clears(&fixture, &session);
        let note = reopened.wait_for("the recovery note", |record| {
            record["kind"] == "status"
                && record["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains(RECOVERY_NOTE))
        });
        let detail = note["detail"].as_str().expect("a note detail");
        for expected in RUNNING_TOOL_ONLY {
            assert!(detail.contains(expected), "note was:\n{detail}");
        }
        reopened.send(&prompt_frame("carry on after the recovery"));
        reopened.wait_for("assistant_done", |record| {
            record["kind"] == "assistant_done"
        });
        reopened.send(&quit_frame());
        let outcome = reopened.finish();
        assert!(outcome.status.success(), "stderr:\n{}", outcome.stderr);
        assert_eq!(
            std::fs::read_to_string(&log).expect("call log"),
            "hang\n",
            "the claimed call was dispatched again"
        );
    }

    /// D: one nested call finished, one still running, then interrupt.
    ///
    /// Accepted-and-usable and recovery-required-and-refused are both legal.
    /// A hang, an acceptance while show is recovery-required, or a missing
    /// receipt of the finished call is not. Join time is reported, and a
    /// join that waits out `sleep 31` fails.
    #[test]
    fn durability_code_mode_cell_partial_nested_completion() {
        let fixture = Fixture::new(&partial_cell_script());
        let mut agent = AgentProcess::start(&fixture, &[]);
        agent.wait_for("ready", |record| record["kind"] == "ready");
        agent.send(&prompt_frame("run a partial cell"));
        wait_for_fixture_file(&fixture.workspace().join("nested-done.txt"));
        wait_for_fixture_file(&fixture.workspace().join("sleep.pid"));
        let started = Instant::now();
        let watermark = agent.snapshot().len();
        agent.send(&json!({"v": 1, "kind": "interrupt"}));
        let settled = wait_for_new(
            &agent,
            watermark,
            "the partial cell settled",
            Duration::from_secs(20),
            |record| {
                let kind = record.get("kind").and_then(Value::as_str);
                kind == Some("assistant_done")
                    || (kind == Some("turn_lifecycle")
                        && record.get("phase").and_then(Value::as_str) != Some("admitted"))
            },
        );
        let join = started.elapsed();
        assert!(
            join < Duration::from_secs(20),
            "interrupt join took {}ms (bound 20000) and did not cut the running nested call; last record {settled}",
            join.as_millis()
        );

        let follow_from = agent.snapshot().len();
        agent.send(&prompt_frame("the next prompt after the partial cell"));
        let follow = wait_for_new(
            &agent,
            follow_from,
            "the next prompt settled",
            WAIT,
            |record| {
                record["kind"] == "assistant_done"
                    || record["kind"] == "error"
                    || (record["kind"] == "status"
                        && record.to_string().to_ascii_lowercase().contains("recovery"))
            },
        );
        agent.send(&quit_frame());
        let outcome = agent.finish();
        let stream = outcome.lines.join("\n");
        let accepted = stream.contains(PARTIAL_ANSWER);
        let session = outcome.session_id();
        let show = show_session(&fixture, &session);
        let recovery = !show.status.success()
            || show.stdout.contains("recovery required")
            || show.stderr.contains("recovery");
        let receipt = std::fs::read_to_string(fixture.workspace().join("nested-done.txt"))
            .unwrap_or_default();
        assert!(
            receipt.contains("nested-done"),
            "the finished nested call left no receipt"
        );
        if accepted {
            assert!(
                !recovery,
                "the next prompt was accepted while session show is recovery-required \
                 (interrupt join {}ms). show exit {:?} stdout:\n{}\nstderr:\n{}\nstream:\n{stream}",
                join.as_millis(),
                show.status.code(),
                show.stdout,
                show.stderr
            );
        } else {
            assert!(
                recovery || follow.to_string().to_ascii_lowercase().contains("recovery"),
                "the next prompt was neither accepted nor refused with recovery named \
                 (interrupt join {}ms). follow {follow} show:\n{}\n{stream}",
                join.as_millis(),
                show.stdout
            );
        }
        #[allow(
            clippy::print_stderr,
            reason = "a passing run must still record which outcome the cell produced and how long the interrupt took to join"
        )]
        {
            eprintln!(
                "code-mode partial cell outcome={} join_ms={}",
                if accepted {
                    "accepted"
                } else {
                    "recovery-refused"
                },
                join.as_millis()
            );
        }
        let _ = follow;
    }

    fn failure_wave_script() -> Value {
        json!({
            "version": 1,
            "tree": true,
            "durable_tree": true,
            "fail_child_after_successes": 2,
            "steps": [
                tool_step("spawn_agent", &spawn_arguments("wave one")),
                tool_step("wait_agent", &json!({"agent_ids": ["$result.agent_id"]})),
                tool_step("adopt_candidate", &json!({"candidate_id": "$result.candidate"})),
                tool_step("spawn_agent", &spawn_arguments("wave two")),
                tool_step("wait_agent", &json!({"agent_ids": ["$result.agent_id"]})),
                tool_step("adopt_candidate", &json!({"candidate_id": "$result.candidate"})),
                tool_step("spawn_agent", &spawn_arguments("wave three fails")),
                text_step(ROOT_NAMES_FAILURE),
                text_step(HONEST_NEXT)
            ]
        })
    }

    fn jsonl_failure_wave(fixture: &Fixture) {
        let mut agent = AgentProcess::start(fixture, &[]);
        agent.wait_for("ready", |record| record["kind"] == "ready");
        agent.send(&prompt_frame("run the failure wave"));
        agent.wait_for("the wave's root answer", |record| {
            record["kind"] == "assistant_done"
        });
        let midway = agent.snapshot().join("\n");
        assert!(
            midway.contains(FAILURE_TEXT) && midway.contains(ROOT_NAMES_FAILURE),
            "the failure wave was not visible before the next prompt:\n{midway}"
        );
        let follow_from = agent.snapshot().len();
        agent.send(&prompt_frame("ask the next root"));
        wait_for_new(
            &agent,
            follow_from,
            "the next root settled",
            WAIT,
            |record| {
                record["kind"] == "assistant_done"
                    || record["kind"] == "error"
                    || (record["kind"] == "status"
                        && record.to_string().to_ascii_lowercase().contains("recovery"))
            },
        );
        agent.send(&quit_frame());
        let outcome = agent.finish();
        assert!(
            outcome.status.success()
                || outcome.lines.iter().any(|line| line.contains(FAILURE_TEXT)),
            "jsonl wave did not finish; stderr:\n{}",
            outcome.stderr
        );
        let show = show_session(fixture, &outcome.session_id());
        assert!(
            show.stdout.contains("node=RecoveryRequired"),
            "the failed child is not recovery-required on show:\n{}",
            show.stdout
        );
    }

    fn tui_failure_wave(fixture: &Fixture) {
        let mut terminal = TerminalChild::spawn(fixture, 120, None);
        terminal.wait_screen("/10M");
        terminal.send(b"run the failure wave\r");
        // Child milestone rows end each group, so the failed spawn is its own
        // one-call group followed by the child's needs-recovery milestone.
        // `scripted_child_failure` stays the JSONL and event-log oracle below.
        terminal.wait_where("the failure is on screen", |frame| {
            frame.contains(ROOT_NAMES_FAILURE)
                && frame.contains("✗ Spawn agent wave three fails")
                && frame.contains("⚠ c3 needs recovery")
        });
        terminal.send(b"ask the next root\r");
        terminal.wait_where("the next root settled", |frame| {
            frame.contains(HONEST_NEXT)
                || frame.to_ascii_lowercase().contains("recovery")
                || frame.contains("RecoveryRequired")
        });
        terminal.send(b"\x11");
        assert!(terminal.finish().success(), "tui wave did not quit");
        let session = saved_session_id(fixture);
        let events = session_events(fixture);
        assert!(
            events.contains(FAILURE_TEXT),
            "the failure wave is missing from the event log:\n{events}"
        );
        let show = show_session(fixture, &session);
        assert!(
            show.stdout.contains("node=RecoveryRequired"),
            "the failed child is not recovery-required on show:\n{}",
            show.stdout
        );
    }

    /// Two successful child waves, then a failed child and a following root
    /// prompt. Both faces retain the failure evidence; accepting a new root
    /// does not depend on whether every child locator can be inspected.
    #[test]
    fn durability_subagent_failure_wave_is_visible_on_both_faces() {
        let jsonl_home = Fixture::new(&failure_wave_script());
        let tui_home = Fixture::new(&failure_wave_script());
        jsonl_failure_wave(&jsonl_home);
        tui_failure_wave(&tui_home);
    }
}

#[path = "offline_e2e/tui_view_parity.rs"]
mod tui_view_parity;

// ---------------------------------------------------------------------------
// Image prompts (`plak run --image`)
// ---------------------------------------------------------------------------

/// A minimal PNG header the image sniffer accepts, padded to `extra` bytes.
fn png_file(fixture: &Fixture, name: &str, extra: usize) -> PathBuf {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&64_u32.to_be_bytes());
    png.extend_from_slice(&48_u32.to_be_bytes());
    png.resize(png.len() + extra, 0);
    let path = fixture.root.join(name);
    std::fs::write(&path, png).unwrap();
    path
}

fn image_run(fixture: &Fixture, images: &[&Path]) -> CliRun {
    let mut arguments = vec![
        "run",
        "--account",
        "codex",
        "--model",
        "offline-loopback-v1",
        "--effort",
        "medium",
    ];
    let images = images
        .iter()
        .map(|path| path.to_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    for image in &images {
        arguments.extend(["--image", image.as_str()]);
    }
    arguments.extend(["--", "what is in these?"]);
    run_cli(fixture, &arguments)
}

#[test]
fn a_cli_image_prompt_reaches_a_model_that_accepts_images() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "image_input": true,
        "steps": [{"output": "image_report"}]
    }));
    let first = png_file(&fixture, "first.png", 100);
    // A photo-sized image: canonical identities must not apply the tool
    // schema bounds to media bytes.
    let second = png_file(&fixture, "second.png", 300_000);
    let run = image_run(&fixture, &[&first, &second]);
    assert!(
        run.status.success(),
        "run failed: {}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout
            .contains("images seen: [image/png 124 bytes, image/png 300024 bytes]"),
        "the model did not receive both images in order: {}",
        run.stdout
    );
}

#[test]
fn a_cli_image_prompt_on_a_model_without_image_input_is_refused_naming_the_route() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [{"output": "image_report"}]
    }));
    let image = png_file(&fixture, "shot.png", 10);
    let run = image_run(&fixture, &[&image]);
    assert!(
        !run.status.success(),
        "an image prompt was admitted: {}",
        run.stdout
    );
    assert!(
        run.stderr.contains("does not accept image input")
            && run.stderr.contains("offline-loopback-v1"),
        "refusal did not name the route and model: {}",
        run.stderr
    );
    assert!(!run.stdout.contains("images seen"), "{}", run.stdout);

    let missing = fixture.root.join("missing.png");
    let run = image_run(&fixture, &[&missing]);
    assert_eq!(run.status.code(), Some(2), "{}", run.stderr);
    assert!(run.stderr.contains("missing.png"), "{}", run.stderr);
}

#[test]
fn http_rpc_refusal_is_a_definite_tool_failure() {
    let fixture = Fixture::new(&json!({"version":1,"steps":[
        {"output":{"tool":{"name":"mcp__fake__rpc_error","arguments":{}}},"finish":"tool_calls"},
        {"output":{"text":"continued after HTTP refusal"}}
    ]}));
    let mut server = Command::new(fake_mcp_server())
        .arg("--http")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line.trim().strip_prefix("listening on ").unwrap();
    let config = fixture.home().join("config.toml");
    let mut body = std::fs::read_to_string(&config).unwrap();
    write!(
        body,
        "\n[mcp_servers.fake]\ntransport = \"http\"\nurl = {url:?}\n"
    )
    .unwrap();
    write_private(&config, &body);
    let lead = fixture.home().join("agents/lead.toml");
    let mut body = std::fs::read_to_string(&lead).unwrap();
    body.push_str("tools = [\"mcp:fake\"]\n");
    write_private(&lead, &body);
    let outcome = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "call the tool",
        ],
    );
    let _ = server.kill();
    let _ = server.wait();
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert!(outcome.stdout.contains("continued after HTTP refusal"));
    let log = run_cli(
        &fixture,
        &[
            "session",
            "log",
            &outcome.session_id(),
            "--kind",
            "tool",
            "--json",
        ],
    );
    assert!(
        log.stdout.contains("scripted RPC refusal"),
        "{}",
        log.stdout
    );
    assert!(log.stdout.contains("-32021"), "{}", log.stdout);
}

#[test]
fn current_store_ambiguity_is_not_hidden_by_a_foreign_session() {
    use plak::store::session_records::SessionRecordStore as _;

    let fixture = Fixture::new(&json!({"version":1,"steps":[]}));
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let store = plak::store::session_records::SqliteSessionStore::new(paths);
    for id in [
        "00000000-0000-4000-8000-000000000041",
        "00000000-0000-4000-8000-000000000042",
    ] {
        let mut record = plak::store::sessions::SessionRecord::new(fixture.workspace());
        record.id = id.to_owned();
        record.create_operation_id = format!("plak:create:{id}");
        store.save(&mut record).unwrap();
    }
    let foreign = fixture.home().join("state/r1.e1.p1.t1/sessions");
    std::fs::create_dir_all(&foreign).unwrap();
    write_private(
        &foreign.join("00000000-0000-4000-8000-000000000043.events.jsonl"),
        "",
    );
    for args in [
        vec!["run", "--session", "00000000", "--", "continue"],
        vec!["session", "show", "00000000"],
    ] {
        let refusal = run_cli(&fixture, &args);
        assert!(
            refusal.stderr.contains("matches 2 sessions"),
            "{}",
            refusal.stderr
        );
        assert!(
            !refusal.stderr.contains("incompatible plak build"),
            "{}",
            refusal.stderr
        );
    }
}
