//! Depth coverage of the terminal application on a real pseudoterminal.
//!
//! Every test spawns the shipped `plak` binary on a PTY against the scripted
//! offline model (`crate::test_hooks`, feature `test-hooks`) and asserts what
//! the screen shows, read back through a `vt100` parser fed by the PTY. The
//! state machine behind the frames is never consulted: if the operator would
//! not see it, the test does not see it either.
//!
//! `bin/plak/tests/offline_e2e.rs` owns the three faces' lifecycles; this file
//! is the terminal face in depth — multi-turn editing sessions, long-transcript
//! navigation, compaction, crash recovery, resizing, every documented
//! keybinding and the session picker.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private process-level test harness"
)]

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};

/// Wall bound for one scripted lifecycle step. Generous slack for process
/// start and `SQLite` open under gate load, not a semantic allowance.
const WAIT: Duration = Duration::from_secs(45);

/// The stall one mid-turn interruption needs: long enough that a signal or a
/// chord lands inside the provider operation on a loaded machine.
const MID_TURN_STALL_MS: u64 = 3_000;

/// A longer stall for the queue chords, which need several keystrokes to land
/// while the turn is still busy.
const QUEUE_STALL_MS: u64 = 16_000;

const TUI_ROWS: u16 = 30;
const TUI_COLS: u16 = 100;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// One disposable product home, workspace and script document.
struct Fixture {
    root: PathBuf,
}

static FIXTURE_ORDINAL: AtomicU32 = AtomicU32::new(0);

impl Fixture {
    fn new(script: &Value) -> Self {
        let ordinal = FIXTURE_ORDINAL.fetch_add(1, Ordering::Relaxed);
        // Under `TMPDIR`, like every other test's scratch, so the gate's
        // storage placement (`PLAK_TEST_TMPDIR`) reaches these sessions too.
        let root = std::env::temp_dir()
            .join("offline-tui")
            .join(format!("{}-{ordinal}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fixture = Self { root };
        std::fs::create_dir_all(fixture.home().join("agents")).expect("home");
        std::fs::create_dir_all(fixture.workspace()).expect("workspace");
        std::fs::create_dir_all(fixture.root.join("tmp")).expect("tmp");
        write_private(
            &fixture.home().join("config.toml"),
            "[root]\nagent = \"lead\"\n\n[orchestration]\nsubagents = false\n\n[compaction]\nenabled = true\n",
        );
        write_private(
            &fixture.home().join("agents").join("lead.toml"),
            "account = \"codex\"\nmodel = \"offline-loopback-v1\"\neffort = \"medium\"\ncomposition = \"direct\"\ndescription = \"offline loopback lead\"\n",
        );
        fixture.write_script(script);
        std::fs::set_permissions(fixture.home(), std::fs::Permissions::from_mode(0o700))
            .expect("home mode");
        std::fs::set_permissions(
            fixture.home().join("agents"),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("agents mode");
        fixture
    }

    /// Replaces the script document; the next process reads the new one.
    fn write_script(&self, script: &Value) {
        std::fs::write(
            self.script(),
            serde_json::to_vec_pretty(script).expect("script json"),
        )
        .expect("script file");
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn script(&self) -> PathBuf {
        self.root.join("script.json")
    }

    fn evidence(&self) -> PathBuf {
        self.root.join("evidence.jsonl")
    }

    /// Blocks until the offline model has entered its provider operation: the
    /// fsynced evidence log gets its first row when the model announces the
    /// dispatch. A fixed sleep would race process start under gate load.
    fn wait_for_provider_dispatch(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            if std::fs::read(self.evidence()).is_ok_and(|bytes| !bytes.is_empty()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the offline model never entered its provider operation"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Forgets the evidence rows an earlier process wrote, so the next
    /// dispatch wait is for the next process.
    fn reset_evidence(&self) {
        let _ = std::fs::remove_file(self.evidence());
    }

    /// The session ids `plak session list` prints for this workspace, read
    /// through the CLI face because the screen only ever shows a short prefix.
    fn session_ids(&self) -> Vec<String> {
        let output = Command::new(env!("CARGO_BIN_EXE_plak"))
            .args(["session", "list"])
            .current_dir(self.workspace())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("fake-home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("PLAK_HOME", self.home())
            .env("PLAK_TEST_MODEL", "offline-loopback-v1")
            .env("PLAK_TEST_SCRIPT", self.script())
            .env("PLAK_TEST_EVIDENCE", self.evidence())
            .output()
            .expect("plak session list spawned");
        let text = String::from_utf8_lossy(&output.stdout);
        let mut ids: Vec<String> = text
            .split(|c: char| c.is_whitespace() || c == '|')
            .filter(|token| looks_like_uuid(token))
            .map(str::to_owned)
            .collect();
        ids.dedup();
        assert!(
            !ids.is_empty(),
            "plak session list named no session; stdout:\n{text}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        ids
    }
}

impl Fixture {
    /// One CLI command in this fixture, read to completion.
    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_plak"))
            .args(args)
            .current_dir(self.workspace())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("fake-home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("PLAK_HOME", self.home())
            .env("PLAK_TEST_MODEL", "offline-loopback-v1")
            .env("PLAK_TEST_SCRIPT", self.script())
            .env("PLAK_TEST_EVIDENCE", self.evidence())
            .output()
            .expect("plak spawned")
    }

    /// Runs one TUI turn on a fresh session, quits, and returns its id.
    fn session_with_one_turn(&self, prompt: &str, answer: &str) -> String {
        let mut terminal = Terminal::spawn_ready(self);
        terminal.send(format!("{prompt}\r").as_bytes());
        terminal.wait_screen(answer);
        terminal.quit();
        let ids = self.session_ids();
        assert_eq!(ids.len(), 1, "{ids:?}");
        ids[0].clone()
    }

    /// `plak session export <id> --out <root>/<name>`; returns the path.
    fn export(&self, session: &str, name: &str) -> PathBuf {
        let path = self.root.join(name);
        let output = self.cli(&[
            "session",
            "export",
            session,
            "--out",
            path.to_str().expect("utf-8 path"),
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        path
    }
}

fn write_script_file(path: &Path, body: &str) {
    std::fs::write(path, body).expect("script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("script mode");
}

fn looks_like_uuid(token: &str) -> bool {
    token.len() == 36
        && token.char_indices().all(|(index, c)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn write_private(path: &Path, body: &str) {
    std::fs::write(path, body).expect("fixture file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("mode");
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// Three turns, each one tool round trip and then a distinct answer.
fn three_tool_bearing_turns() -> Value {
    json!({
        "version": 1,
        "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": "answer one after the tool"}},
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": "answer two after the tool"}},
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": "answer three after the tool"}}
        ]
    })
}

/// One tool round trip and then an answer, replayed for every turn.
fn tool_then_answer(answer: &str) -> Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": answer}},
            {"output": {"text": answer}}
        ]
    })
}

/// One answer of `lines` numbered lines, replayed for every turn.
fn numbered_lines(lines: usize) -> Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": (0..lines)
            .map(|line| format!("transcript line {line:05}"))
            .collect::<Vec<_>>()
            .join("\n")}}]
    })
}

/// One answer preceded by a stall long enough to interrupt, then plain
/// answers for every later turn.
fn stalled_then_answers(stall_ms: u64) -> Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [
            {"output": {"text": "the stalled answer"}, "stall_ms": stall_ms},
            {"output": {"text": "the answer after the stall"}}
        ]
    })
}

/// The growth script: two large tool results either side of the fence, then an
/// answer for every later turn. With `fold: refuse` the compaction ladder takes
/// its offline placeholder rung, which is the note this file asserts on.
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

/// A tree turn: spawn a child, wait for it, adopt its candidate, answer.
fn tree_turn_script() -> Value {
    json!({
        "version": 1,
        "tree": true,
        "repeat_last_step": true,
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
    })
}

fn recovery_answer_script() -> Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": "recovered and answered"}}]
    })
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

/// Bytes `vt100` folds into a screen, plus the raw stream for the sequences a
/// screen cannot show (OSC 52 clipboard writes, the alternate-screen exit).
struct Capture {
    parser: vt100::Parser,
    raw: Vec<u8>,
}

/// One real terminal running the shipped binary on a pseudoterminal.
struct Terminal {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    capture: Arc<Mutex<Capture>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

/// How the terminal is launched.
#[derive(Clone, Copy)]
enum Launch<'a> {
    /// A fresh session in the fixture workspace.
    Fresh,
    Faulted {
        resume: Option<&'a str>,
        faults: &'a str,
    },
    /// `plak --resume`: the launch picker over this workspace's sessions.
    Picker,
    /// `plak --resume <id>`: one named session.
    Resume(&'a str),
    /// The binary with exactly these arguments.
    Args(&'a [&'a str]),
    /// Started as `plak` found on a `PATH` whose first entry is the given
    /// directory, so a `plak` placed there later is what `/reload` finds.
    OnPath(&'a Path),
}

impl Terminal {
    fn spawn(fixture: &Fixture, launch: Launch<'_>) -> Self {
        Self::spawn_sized(fixture, launch, TUI_ROWS, TUI_COLS)
    }

    fn spawn_sized(fixture: &Fixture, launch: Launch<'_>, rows: u16, cols: u16) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pty opened");
        let inherited = std::env::var("PATH").unwrap_or_default();
        let (mut command, path) = match launch {
            Launch::OnPath(first) => {
                let real = fixture.root.join("real-bin");
                std::fs::create_dir_all(&real).expect("real bin dir");
                let link = real.join("plak");
                if !link.exists() {
                    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_plak"), &link)
                        .expect("plak link");
                }
                (
                    CommandBuilder::new("plak"),
                    format!("{}:{}:{inherited}", first.display(), real.display()),
                )
            }
            _ => (CommandBuilder::new(env!("CARGO_BIN_EXE_plak")), inherited),
        };
        match launch {
            Launch::Fresh | Launch::OnPath(_) | Launch::Faulted { resume: None, .. } => {}
            Launch::Faulted {
                resume: Some(id), ..
            }
            | Launch::Resume(id) => command.args(["--resume", id]),
            Launch::Picker => command.args(["--resume"]),
            Launch::Args(args) => command.args(args),
        }
        command.env_clear();
        command.env("PATH", path);
        command.env("HOME", fixture.root.join("fake-home"));
        command.env("TMPDIR", fixture.root.join("tmp"));
        command.env("PLAK_HOME", fixture.home());
        if let Launch::Faulted { faults, .. } = launch {
            command.env("PLAK_TEST_FAULTS", faults);
        }
        command.env("PLAK_TEST_MODEL", "offline-loopback-v1");
        command.env("PLAK_TEST_SCRIPT", fixture.script());
        command.env("PLAK_TEST_EVIDENCE", fixture.evidence());
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
        let capture = Arc::new(Mutex::new(Capture {
            parser: vt100::Parser::new(rows, cols, 0),
            raw: Vec::new(),
        }));
        let sink = Arc::clone(&capture);
        let reader_thread = std::thread::spawn(move || {
            let mut buffer = [0_u8; 16384];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let mut capture = sink.lock().unwrap();
                capture.parser.process(&buffer[..count]);
                capture.raw.extend_from_slice(&buffer[..count]);
            }
        });
        Self {
            master: pair.master,
            writer,
            child,
            capture,
            reader: Some(reader_thread),
        }
    }

    /// Launches and waits for the ready shell: bound tuple and composer.
    fn spawn_ready(fixture: &Fixture) -> Self {
        let mut terminal = Self::spawn(fixture, Launch::Fresh);
        terminal.wait_ready();
        terminal
    }

    fn wait_ready(&mut self) -> String {
        self.wait_where("the ready shell", |frame| {
            frame.contains("%/") && frame.contains("/ commands")
        })
    }

    fn screen(&self) -> String {
        self.capture.lock().unwrap().parser.screen().contents()
    }

    fn raw_len(&self) -> usize {
        self.capture.lock().unwrap().raw.len()
    }

    fn raw_since(&self, from: usize) -> Vec<u8> {
        self.capture.lock().unwrap().raw[from..].to_vec()
    }

    /// The cursor's `(row, column)` on the current screen.
    fn cursor(&self) -> (u16, u16) {
        self.capture
            .lock()
            .unwrap()
            .parser
            .screen()
            .cursor_position()
    }

    fn pid(&self) -> u32 {
        self.child.process_id().expect("child pid")
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    /// Sends to a child that may exit at any moment: once it has, the pty
    /// write fails (EIO on macOS) and that is not a test failure.
    fn send_until_exit(&mut self, bytes: &[u8]) {
        let sent = self
            .writer
            .write_all(bytes)
            .and_then(|()| self.writer.flush());
        if let Err(error) = sent {
            // The pty closes as the child exits, just before it is reapable.
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.is_alive() {
                assert!(
                    Instant::now() < deadline,
                    "pty write to a live child: {error}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pty resized");
        self.capture.lock().unwrap().parser.set_size(rows, cols);
    }

    fn is_alive(&mut self) -> bool {
        self.child.try_wait().expect("wait").is_none()
    }

    /// Waits for a stable frame matching `predicate`.
    fn wait_where(&mut self, description: &str, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
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

    fn wait_absent(&mut self, needle: &str) -> String {
        self.wait_where(&format!("a screen without `{needle}`"), |frame| {
            !frame.contains(needle)
        })
    }

    /// Walks ↑/↓ until the focused Menu row is `name` (`▾ name`).
    fn focus_control_row(&mut self, name: &str) {
        let needle = format!("▾ {name}");
        if self.screen().contains(&needle) {
            return;
        }
        for key in [b"\x1b[B".as_slice(), b"\x1b[A".as_slice()] {
            for _ in 0..16 {
                self.send(key);
                let frame = self.wait_where("a focused Menu row", |frame| {
                    frame.lines().any(|line| line.contains('▾'))
                });
                if frame.contains(&needle) {
                    return;
                }
            }
        }
        panic!("never focused the `{name}` Menu row:\n{}", self.screen());
    }

    /// Types `text` and waits until the composer shows it on a stable frame.
    fn type_draft(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.wait_screen(text);
    }

    /// The idle Esc-Esc pair, retried: the second Esc must land inside the
    /// arming window, which a loaded machine can miss, and re-arming is
    /// idempotent.
    fn esc_esc_clear_draft(&mut self, draft: &str) {
        for _ in 0..5 {
            self.send(b"\x1b");
            std::thread::sleep(Duration::from_millis(40));
            self.send(b"\x1b");
            let deadline = Instant::now() + Duration::from_millis(600);
            while Instant::now() < deadline {
                if !self.screen().contains(draft) {
                    return;
                }
                assert!(self.is_alive(), "the terminal exited during Esc-Esc");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        panic!(
            "Esc-Esc never cleared the draft `{draft}`\n{}",
            self.screen()
        );
    }

    fn finish(mut self) -> portable_pty::ExitStatus {
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
        status
    }

    fn quit(mut self) {
        self.send(b"/quit\r");
        let status = self.finish();
        assert!(status.success(), "the terminal did not exit cleanly");
    }

    /// `SIGKILL`, which no handler can soften; returns the exit status.
    fn kill_hard(mut self) -> portable_pty::ExitStatus {
        let status = Command::new("kill")
            .arg("-9")
            .arg(self.pid().to_string())
            .status()
            .expect("kill spawned");
        assert!(status.success(), "kill -9 failed");
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                break status;
            }
            assert!(Instant::now() < deadline, "a killed terminal never exited");
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(self.master);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        status
    }
}

/// The screen row that holds `needle`.
fn row_of(frame: &str, needle: &str) -> usize {
    frame
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` is not on screen:\n{frame}"))
}

fn is_tool_row(line: &str) -> bool {
    line.contains("✓ Loopback effect")
        && line.contains('✓')
        && !line.contains("│ ")
        && !line.contains("└ ")
}

// ---------------------------------------------------------------------------
// Case 1: a multi-turn editing session
// ---------------------------------------------------------------------------

/// Three prompts, each with a tool call: the transcript shows every prompt,
/// its tool row and its answer, in order, with nothing interleaved.
#[test]
fn three_tool_bearing_turns_render_each_tool_row_and_answer_in_order() {
    let prompts = [
        "edit the first file",
        "edit the second file",
        "edit the third file",
    ];
    let answers = [
        "answer one after the tool",
        "answer two after the tool",
        "answer three after the tool",
    ];
    let fixture = Fixture::new(&three_tool_bearing_turns());
    let mut terminal = Terminal::spawn_ready(&fixture);
    for (prompt, answer) in prompts.iter().zip(answers) {
        terminal.send(format!("{prompt}\r").as_bytes());
        terminal.wait_screen(answer);
    }
    let frame = terminal.screen();
    let tool_rows: Vec<usize> = frame
        .lines()
        .enumerate()
        .filter(|(_, line)| is_tool_row(line))
        .map(|(row, _)| row)
        .collect();
    assert_eq!(
        tool_rows.len(),
        3,
        "expected one tool row per turn:\n{frame}"
    );
    let mut previous_answer = 0;
    for ((prompt, answer), tool_row) in prompts.iter().zip(answers).zip(tool_rows) {
        let prompt_row = row_of(&frame, prompt);
        let answer_row = row_of(&frame, answer);
        assert!(
            previous_answer < prompt_row && prompt_row < tool_row && tool_row < answer_row,
            "turn `{prompt}` is out of order (prompt {prompt_row}, tool {tool_row}, \
             answer {answer_row}, previous answer {previous_answer}):\n{frame}"
        );
        previous_answer = answer_row;
    }
    terminal.quit();
}

/// A real PTY renders a failed command as one item line with its exit, and
/// never shows output.
#[test]
fn a_command_that_exits_nonzero_renders_a_failed_row_with_its_exit() {
    let fixture = Fixture::new(&json!({
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
            {"output": {"text": "the build failed"}}
        ]
    }));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"build it\r");
    terminal.wait_screen("the build failed");
    let frame = terminal.screen();
    let row = frame
        .lines()
        .find(|line| line.contains("exit 3"))
        .unwrap_or_else(|| panic!("no row names the exit:\n{frame}"));
    assert!(
        row.contains('✗'),
        "the failed command's row: {row}\n{frame}"
    );
    assert!(
        !row.contains('✓'),
        "the failed command's row: {row}\n{frame}"
    );
    // One call keeps its own item line: the command, its exit as meta.
    assert!(row.contains("$ printf"), "{row}\n{frame}");
    assert!(
        !frame.lines().any(|line| line.trim() == "building"),
        "raw output is not admitted: {frame}"
    );
    terminal.quit();
}

// ---------------------------------------------------------------------------
// Case 2: scroll to the head of a long transcript and back
// ---------------------------------------------------------------------------

/// The longest one keystroke may take to move the screen. A key that takes
/// longer than this on an idle machine is what "the TUI lags" means; the
/// budget lane runs this test serially so contention cannot fake it.
const KEY_BOUND: Duration = Duration::from_millis(1_500);

/// Sends `key` until `arrived` holds for the screen, returning the number of
/// presses and the slowest of them. Every press before arrival must move the
/// screen within [`KEY_BOUND`]: the end is recognized by its content, never by
/// a press that seemed to change nothing, so a slow repaint on a loaded host
/// is waited for instead of being read as the end of the document.
fn page_until(
    terminal: &mut Terminal,
    key: &[u8],
    cap: usize,
    arrived: impl Fn(&str) -> bool,
) -> (usize, Duration) {
    let mut moved = 0;
    let mut slowest = Duration::ZERO;
    loop {
        let before = terminal.screen();
        if arrived(&before) {
            return (moved, slowest);
        }
        assert!(
            moved < cap,
            "{key:?} never arrived after {cap} presses:\n{before}"
        );
        let started = Instant::now();
        terminal.send(key);
        while terminal.screen() == before {
            assert!(
                started.elapsed() <= KEY_BOUND,
                "a {key:?} press did not move the screen within {KEY_BOUND:?}\n{before}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        slowest = slowest.max(started.elapsed());
        moved += 1;
    }
}

/// `PageUp` to the head of a long transcript and `PageDown` back to its tail:
/// every keystroke moves the screen within the bound, the banner is back at
/// the head, and the tail is back at the end.
#[test]
fn paging_to_the_head_of_a_long_transcript_and_back_moves_on_every_key() {
    const LINES: usize = 400;
    let fixture = Fixture::new(&numbered_lines(LINES));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"fill the transcript\r");
    let tail = terminal.wait_screen("transcript line 00399");
    assert!(
        !tail.contains("powered by plakous"),
        "the banner must have scrolled away under a long transcript:\n{tail}"
    );

    // Each PageUp moves ten rows; 400 lines need about forty.
    let (ups, slowest_up) = page_until(&mut terminal, b"\x1b[5~", 120, |screen| {
        screen.contains("transcript line 00000") && screen.contains("powered by plakous")
    });
    let head = terminal.wait_screen("transcript line 00000");
    assert!(
        head.contains("powered by plakous"),
        "the head of the transcript must show the session banner again:\n{head}"
    );
    assert!(
        !head.contains("transcript line 00399"),
        "the head and the tail cannot both be on a 30-row screen:\n{head}"
    );

    let (downs, slowest_down) = page_until(&mut terminal, b"\x1b[6~", 120, |screen| {
        screen.contains("transcript line 00399") && screen.contains("/ commands")
    });
    let back = terminal.wait_screen("transcript line 00399");
    assert!(
        back.contains("/ commands"),
        "the composer must still be painted at the tail:\n{back}"
    );
    assert!(ups >= 30 && downs >= 30, "paged {ups} up and {downs} down");
    eprintln!(
        "paged {ups} up ({slowest_up:?} slowest) and {downs} down ({slowest_down:?} slowest)"
    );
    terminal.quit();
}

// ---------------------------------------------------------------------------
// Case 3: compaction mid-session
// ---------------------------------------------------------------------------

/// A turn that crosses the context fence takes the offline placeholder rung
/// mid-turn: the note renders in the transcript, names what it dropped and
/// the projection it recovered, and the session still answers afterwards.
#[test]
fn a_mid_session_compaction_renders_its_note_and_the_session_carries_on() {
    let fixture = Fixture::new(&growth_script("refuse"));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"grow the context\r");
    let frame = terminal.wait_screen("the compacted answer");
    assert!(
        frame.contains("Compact") && (frame.contains('→') || frame.contains("->")),
        "compaction is a Compact transcript row with before → after sizes:\n{frame}"
    );
    assert!(
        !frame.contains("replaced 1 oldest tool result")
            && !frame.contains("compaction replaced")
            && !frame.contains("compact workflow committed"),
        "a finished compact is the Compact row, not a grey replacement essay:\n{frame}"
    );
    assert!(
        !frame
            .lines()
            .any(|line| line.contains("Compact") && line.contains("failed")),
        "no failed Compact row may precede the answer:\n{frame}"
    );
    let note_row = row_of(&frame, "Compact");
    let answer_row = row_of(&frame, "the compacted answer");
    let group_row = row_of(&frame, "Loopback effect ×2");
    assert!(
        group_row < note_row && note_row < answer_row,
        "the activity summary precedes standalone compaction and prose: {frame}"
    );
    assert!(
        !frame.contains("❯ [plak:") && !frame.contains("❯ replaced 1 oldest"),
        "the harness note must never render as a typed prompt:\n{frame}"
    );

    terminal.send(b"carry on after the compaction\r");
    terminal.wait_where("a turn after the compaction", |frame| {
        frame.contains("carry on after the compaction")
            && frame.matches("the compacted answer").count() >= 2
    });
    terminal.quit();
}

/// The history navigator labels the placeholder pass note `harness`, never `user`.
///
/// HELD: the scripted model attaches no topology hub, so Ctrl+T answers
/// `agent tree navigator unavailable` and the node transcript overlay that
/// pages canonical history cannot open. The assertion is the product's own
/// (`topology_navigator.rs`, `transcript_actor_label_for`); this runs it end
/// to end once the harness binds a hub.
#[test]
#[ignore = "held: the scripted model attaches no topology hub, so the navigator cannot open"]
fn the_history_navigator_labels_the_placeholder_note_harness_not_user() {
    let fixture = Fixture::new(&growth_script("refuse"));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"grow the context\r");
    terminal.wait_screen("the compacted answer");
    terminal.send(b"\x14");
    terminal.wait_where("the agent canvas", |frame| {
        !frame.contains("requesting tree snapshot") && frame.contains("root")
    });
    terminal.send(b"\x1b[H");
    terminal.send(b"\r");
    let page = terminal.wait_screen("transcript · ");
    assert!(
        page.contains("[harness]"),
        "the paged transcript must label the placeholder pass note harness:\n{page}"
    );
    assert!(
        !page.contains("[user] [plak:") && !page.contains("[user] replaced 1 oldest"),
        "the placeholder pass note must not read as something the operator typed:\n{page}"
    );
    terminal.send(b"\x1b");
    terminal.send(b"q");
    terminal.quit();
}

// ---------------------------------------------------------------------------
// Case 4: a tree with a child
// ---------------------------------------------------------------------------

/// A turn that spawns a child, waits for it and adopts its candidate renders
/// the three topology tool rows and the answer.
///
/// The `tree` script dial is the only child route from a spawned process; it
/// is not the composition (Code Mode) route, which the scripted binding does
/// not combine with a child binder. The canvas half is held with the
/// navigator test above: no hub, no canvas.
#[test]
fn a_tree_turn_renders_its_spawn_wait_and_adopt_rows_and_answers() {
    let fixture = Fixture::new(&tree_turn_script());
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"delegate this\r");
    let frame = terminal.wait_where("the adopted answer on an idle shell", |frame| {
        frame.contains("the child's work is adopted")
            && !frame.contains("thinking")
            && !frame.contains("waiting for model")
    });
    let rows = tree_turn_activity_rows(&frame)
        .unwrap_or_else(|| panic!("missing completed tree activity on screen:\n{frame}"));
    assert!(
        rows.windows(2).all(|pair| pair[0] < pair[1])
            && rows.last().unwrap() < &row_of(&frame, "the child's work is adopted"),
        "the tree rows must render in call order:\n{frame}"
    );
    assert!(
        !frame.contains("1 agent working"),
        "the completed turn must not show its adopted child as working:\n{frame}"
    );
    assert!(
        !frame.contains("$result."),
        "a tree placeholder reached the screen unresolved:\n{frame}"
    );
    terminal.quit();
}

fn tree_turn_activity_rows(frame: &str) -> Option<Vec<usize>> {
    // Settled snapshots may fold all three calls into one completed group;
    // a live child's milestone can instead split that same activity in two.
    if let Some(row) = frame
        .lines()
        .position(|line| line.contains("✓ Spawned 1 agent, 1 agent finished, adopted 1 candidate"))
    {
        return Some(vec![row]);
    }
    // An adoption milestone that lands between the wait and the adopt call
    // splits them into separate groups.
    [
        &[
            "✓ Spawn agent",
            "✓ c1 finished",
            "✓ 1 agent finished, adopted 1 candidate",
        ][..],
        &[
            "✓ Spawn agent",
            "✓ c1 finished",
            "✓ 1 agent finished",
            "✓ Adopt cand-c1-a1",
        ][..],
    ]
    .iter()
    .find_map(|names| {
        names
            .iter()
            .map(|name| frame.lines().position(|line| line.contains(name)))
            .collect()
    })
}

#[test]
fn tree_turn_row_oracle_accepts_settled_folding() {
    let frame = "  ✓ Spawned 1 agent, 1 agent finished, adopted 1 candidate  ▸\n\
                 the child's work is adopted";
    assert_eq!(tree_turn_activity_rows(frame), Some(vec![0]));
    assert_eq!(
        tree_turn_activity_rows(
            "✓ Spawn agent\n✓ c1 finished\n✓ 1 agent finished, adopted 1 candidate"
        ),
        Some(vec![0, 1, 2])
    );
    assert_eq!(
        tree_turn_activity_rows(
            "✓ Spawn agent\n◆ spawned c1\n✓ c1 finished\n✓ 1 agent finished\n\
             ⇲ adopted cand-c1-a1\n✓ Adopt cand-c1-a1"
        ),
        Some(vec![0, 2, 3, 5])
    );
    for incomplete in [
        "✓ Spawned 1 agent, adopted 1 candidate",
        "✓ Spawned 1 agent, 1 agent finished",
        "✗ Spawned 1 agent, 1 agent finished, adopted 1 candidate (1 failed)",
    ] {
        assert_eq!(tree_turn_activity_rows(incomplete), None);
    }
}

/// Ctrl+T after a tree turn shows the root and its one child; `q` leaves.
#[test]
fn the_agent_canvas_shows_the_child_a_tree_turn_spawned() {
    let fixture = Fixture::new(&tree_turn_script());
    let mut terminal = Terminal::spawn_sized(&fixture, Launch::Fresh, 45, TUI_COLS);
    terminal.wait_ready();
    terminal.send(b"delegate this\r");
    terminal.wait_screen("the child's work is adopted");
    terminal.send(b"\x14");
    let canvas = terminal.wait_where("the agent canvas with a child", |frame| {
        frame.contains("Activity")
            && frame.contains("#c1")
            && !frame.contains("requesting tree snapshot")
    });
    assert!(canvas.contains("root"), "{canvas}");
    terminal.send(b"\x1b");
    terminal.wait_absent("Activity");
    terminal.quit();
}

// ---------------------------------------------------------------------------
// Case 5: crash recovery
// ---------------------------------------------------------------------------

/// Reopens the killed session by id until the lease the dead process held has
/// cleared, and returns the terminal with the recovery note on screen.
fn reopen_after_kill(fixture: &Fixture, session: &str) -> Terminal {
    let deadline = Instant::now() + WAIT;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let mut terminal = Terminal::spawn(fixture, Launch::Resume(session));
        let limit = Instant::now() + Duration::from_secs(10);
        let recovered = loop {
            let frame = terminal.screen();
            if frame.contains("recovered an interrupted turn") && frame.contains("/ commands") {
                break true;
            }
            if !terminal.is_alive() || Instant::now() > limit {
                break false;
            }
            std::thread::sleep(Duration::from_millis(40));
        };
        if recovered {
            return terminal;
        }
        let last = terminal.screen();
        if terminal.is_alive() {
            let _ = terminal.kill_hard();
        }
        assert!(
            Instant::now() < deadline,
            "the session never reopened with its recovery note after {attempts} attempts; \
             the last screen was:\n{last}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A terminal killed with `SIGKILL` mid-turn: the relaunched terminal on the
/// same session renders the recovery note exactly once and answers the next
/// prompt.
#[test]
fn a_terminal_killed_mid_turn_relaunches_with_one_recovery_note_and_answers() {
    let fixture = Fixture::new(&stalled_then_answers(MID_TURN_STALL_MS));
    let mut first = Terminal::spawn_ready(&fixture);
    first.send(b"stall so the kill lands mid-turn\r");
    fixture.wait_for_provider_dispatch();
    let status = first.kill_hard();
    assert!(!status.success(), "a killed terminal cannot exit cleanly");
    let ids = fixture.session_ids();
    assert_eq!(ids.len(), 1, "one session in the workspace: {ids:?}");
    let session = ids[0].clone();

    fixture.reset_evidence();
    fixture.write_script(&recovery_answer_script());
    let mut reopened = reopen_after_kill(&fixture, &session);
    // The recovery note is published before the presentation snapshot, so a
    // frame can show the note and the composer while the restored prompt is
    // still off screen. Wait until one stable frame holds both.
    let prefix = &session[..8];
    let frame = reopened.wait_where(
        "the reopened session showing the interrupted prompt and one recovery note",
        |frame| {
            frame.matches("recovered an interrupted turn").count() == 1
                && frame.contains(prefix)
                && frame.contains("stall so the kill lands mid-turn")
        },
    );
    assert_eq!(
        frame.matches("recovered an interrupted turn").count(),
        1,
        "the recovery note must render exactly once:\n{frame}"
    );
    assert!(
        frame.contains(&session[..8]),
        "the reopened terminal must be on the killed session `{session}`:\n{frame}"
    );
    assert!(
        frame.contains("stall so the kill lands mid-turn"),
        "the interrupted prompt must survive the kill:\n{frame}"
    );
    reopened.send(b"carry on after the crash\r");
    let answered = reopened.wait_screen("recovered and answered");
    assert_eq!(
        answered.matches("recovered an interrupted turn").count(),
        1,
        "the next turn must not repeat the recovery note:\n{answered}"
    );
    reopened.quit();
}

/// A turn a live process fenced shows the pending-recovery pill, never a
/// `/clear` dead end; `/recover` reopens the session in place with one
/// operator line, and the next prompt answers.
#[test]
fn a_fenced_turn_shows_the_recovery_pill_and_slash_recover_continues() {
    let fixture = Fixture::new(&json!({
        "version": 1,
        "steps": [
            {"fault": {"at": "in_band", "kind": "outcome_unknown", "after_events": 2}},
            {"output": {"text": "answered after recovery"}}
        ]
    }));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"fence this turn\r");
    let fenced = terminal.wait_screen("needs recovery");
    assert!(
        fenced.contains("next prompt recovers"),
        "the pill says how to continue:\n{fenced}"
    );
    assert!(!fenced.contains("/clear"), "no /clear dead end:\n{fenced}");
    terminal.send(b"/recover\r");
    let recovered = terminal.wait_screen("recovered an interrupted turn");
    assert_eq!(
        recovered.matches("recovered an interrupted turn").count(),
        1,
        "one operator line:\n{recovered}"
    );
    terminal.send(b"carry on\r");
    let answered = terminal.wait_screen("answered after recovery");
    assert!(!answered.contains("needs recovery"), "{answered}");
    terminal.quit();
}

// ---------------------------------------------------------------------------
// Case 6: resize under a long transcript
// ---------------------------------------------------------------------------

/// Every screen line fits the pane width.
fn assert_fits(frame: &str, cols: u16, what: &str) {
    let widest = frame
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    assert!(
        widest <= usize::from(cols),
        "{what}: a line is {widest} cells wide in a {cols}-column pane:\n{frame}"
    );
}

/// The wordmark's first row, present whenever the banner is painted.
const WORDMARK_ROW: &str = "████████╗ ██╗      ██████████╗ ██╗  ██╗";

/// Resizing from 160 to 60 columns and back, first with the banner on screen
/// and then under a 5,000-line transcript: nothing panics, every frame refits
/// the pane, the banner refits with it, and the tail is still the tail.
#[test]
fn resizing_between_160_and_60_columns_under_a_long_transcript_refits_the_banner() {
    const LINES: usize = 5_000;
    let fixture = Fixture::new(&numbered_lines(LINES));
    let mut terminal = Terminal::spawn_sized(&fixture, Launch::Fresh, TUI_ROWS, 160);
    let wide = terminal.wait_ready();
    assert!(
        wide.contains("powered by plakous") && wide.contains(WORDMARK_ROW),
        "the banner must paint at 160 columns:\n{wide}"
    );
    assert_fits(&wide, 160, "the launch frame");
    let rule_at_160 = wide.lines().next().map_or(0, |line| line.chars().count());
    assert_eq!(rule_at_160, 160, "the banner rule spans the pane:\n{wide}");

    terminal.resize(TUI_ROWS, 60);
    let narrow = terminal.wait_where("the 60-column refit", |frame| {
        frame.contains("/ commands")
            && frame
                .lines()
                .next()
                .is_some_and(|line| line.chars().count() == 60)
    });
    assert_fits(&narrow, 60, "the 60-column frame");
    assert!(
        narrow.contains(WORDMARK_ROW),
        "the wordmark still fits a 60-column pane:\n{narrow}"
    );

    terminal.resize(TUI_ROWS, 160);
    let wide_again = terminal.wait_where("the 160-column refit", |frame| {
        frame.contains("powered by plakous")
            && frame
                .lines()
                .next()
                .is_some_and(|line| line.chars().count() == 160)
    });
    assert_fits(&wide_again, 160, "the 160-column frame");

    terminal.send(b"fill the transcript\r");
    let tail = terminal.wait_screen("transcript line 04999");
    assert_fits(&tail, 160, "the long tail at 160 columns");

    terminal.resize(TUI_ROWS, 60);
    let narrow_tail = terminal.wait_where("the 60-column tail", |frame| {
        frame.contains("transcript line 04999")
            && frame.contains("/ commands")
            && frame.lines().all(|line| line.chars().count() <= 60)
    });
    assert_fits(&narrow_tail, 60, "the long tail at 60 columns");

    terminal.resize(TUI_ROWS, 160);
    let wide_tail = terminal.wait_where("the 160-column tail", |frame| {
        frame.contains("transcript line 04999") && frame.contains("%/")
    });
    assert_fits(&wide_tail, 160, "the long tail back at 160 columns");
    assert!(
        terminal.is_alive(),
        "the terminal died across the resizes:\n{wide_tail}"
    );

    // The head still carries the refitted banner after all of it.
    page_until(&mut terminal, b"\x1b[5~", 1_200, |screen| {
        screen.contains("transcript line 00000")
            && screen.contains("powered by plakous")
            && screen.contains(WORDMARK_ROW)
    });
    let head = terminal.wait_screen("transcript line 00000");
    assert!(
        head.contains("powered by plakous") && head.contains(WORDMARK_ROW),
        "the banner must be back at the head, refitted to 160 columns:\n{head}"
    );
    assert_fits(&head, 160, "the head after the resizes");
    terminal.send(b"/quit\r");
    let status = terminal.finish();
    assert!(status.success());
}

// ---------------------------------------------------------------------------
// Case 7: every documented keybinding
// ---------------------------------------------------------------------------

/// The Activity body reports why the offline composition has no agent tree.
const AGENTS_UNAVAILABLE: &str = "Sub-agents are unavailable:";

/// The slash palette's title rule; the composer footer also says
/// `/ commands`, so the box rule is what identifies the palette.
const PALETTE_TITLE: &str = "─ commands ─";

/// Global chords: Ctrl+X, Ctrl+T, Ctrl+O, Alt+O, `/`, Tab in the palette,
/// Esc and Ctrl+Q.
#[test]
fn global_chords_reach_the_control_center_canvas_tool_fold_and_palette() {
    let fixture = Fixture::new(&tool_then_answer("the answer after the tool"));
    let mut terminal = Terminal::spawn_ready(&fixture);

    // Ctrl+X with no selection first opens the Menu on Session.
    terminal.send(b"\x18");
    let session = terminal.wait_screen("Menu");
    assert!(
        session.contains("click a value to copy it") && session.contains("Build"),
        "Ctrl+X must land on the Session tab:\n{session}"
    );
    // Esc closes the focused owner.
    terminal.send(b"\x1b");
    terminal.wait_absent("Menu");

    // Ctrl+T asks the host for the agent canvas; the offline composition has
    // no hub, so Activity reports that in its body, not the transcript.
    terminal.send(b"\x14");
    let after_tree = terminal.wait_where("Ctrl+T reached the host", |frame| {
        frame.contains("Activity") && frame.contains(AGENTS_UNAVAILABLE)
    });
    assert!(after_tree.contains("no topology hub"), "{after_tree}");
    terminal.send(b"\x1b");
    terminal.wait_absent("Activity");

    // Alt+O opens every group and folds them back.
    terminal.send(b"run a tool\r");
    let answered = terminal.wait_screen("the answer after the tool");
    assert!(
        !answered.contains("loopback effect committed"),
        "raw tool output is never admitted:\n{answered}"
    );
    assert!(answered.lines().any(is_tool_row), "{answered}");
    terminal.send(b"\x1bo");
    let opened = terminal.wait_where("Alt+O opens the group", |frame| frame.contains('▾'));
    assert_eq!(
        opened.matches("Loopback effect").count(),
        2,
        "an open group keeps its summary over its item:\n{opened}"
    );
    assert!(!opened.contains("loopback effect committed"), "{opened}");
    terminal.send(b"\x1bo");
    terminal.wait_where("Alt+O folds the group back", |frame| {
        !frame.contains('▾') && frame.matches("Loopback effect").count() == 1
    });

    // `/` opens the palette; typing filters it; Tab completes the highlighted
    // command into the draft; Esc closes the palette and keeps the draft.
    terminal.send(b"/");
    let palette = terminal.wait_screen(PALETTE_TITLE);
    assert!(
        palette.contains("/help") && palette.contains("Show all commands"),
        "the palette must list the commands:\n{palette}"
    );
    terminal.send(b"qu");
    terminal.wait_where("the palette filtered to /quit", |frame| {
        frame.contains("Exit Plak") && !frame.contains("Show all commands")
    });
    terminal.send(b"\t");
    terminal.wait_screen("❯ /quit");
    terminal.send(b"\x1b");
    let closed = terminal.wait_absent(PALETTE_TITLE);
    assert!(
        closed.contains("❯ /quit"),
        "Esc closes the palette and keeps the draft:\n{closed}"
    );
    terminal.esc_esc_clear_draft("/quit");

    // Ctrl+Q quits.
    terminal.send(b"\x11");
    let status = terminal.finish();
    assert!(status.success(), "Ctrl+Q must exit cleanly");
}

/// The Ctrl+C ladder: one press clears a draft, a second press within the
/// window while idle quits.
#[test]
fn ctrl_c_clears_the_draft_and_a_double_press_quits() {
    let fixture = Fixture::new(&tool_then_answer("unused"));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.type_draft("a draft ctrl c will clear");
    terminal.send(b"\x03");
    terminal.wait_absent("a draft ctrl c will clear");
    assert!(terminal.is_alive(), "one Ctrl+C on a draft must not quit");

    terminal.send(b"\x03");
    std::thread::sleep(Duration::from_millis(60));
    terminal.send(b"\x03");
    let status = terminal.finish();
    assert!(
        status.success(),
        "a double Ctrl+C while idle must exit cleanly"
    );
}

/// Esc while busy asks the host to cancel; idle Esc-Esc clears a draft that
/// Ctrl+Z restores and Ctrl+Y clears again.
///
/// The scripted stall is a plain sleep the host's cancel joins rather than
/// drops, so the screen change Esc owns is the `Cancelling…` status, after
/// which the stalled operation settles on its own; a live route would have
/// been cut. The turn's outcome is therefore not asserted here.
#[test]
fn esc_cancels_a_busy_turn_and_esc_esc_clears_an_undoable_draft() {
    const DRAFT: &str = "escesc clears this recoverable draft";
    let fixture = Fixture::new(&stalled_then_answers(MID_TURN_STALL_MS));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"stall so esc can cancel\r");
    fixture.wait_for_provider_dispatch();
    terminal.wait_screen("waiting for model");
    terminal.send(b"\x1b");
    terminal.wait_screen("Stop this turn?");
    // Focus starts on Stop turn, so Enter cancels it.
    terminal.send(b"\r");
    let cancelling = terminal.wait_where("the cancel status", |frame| {
        frame.contains("cancelling…") || frame.contains("⊘ interrupted")
    });
    assert!(
        cancelling.contains("stall so esc can cancel"),
        "the cancelled prompt keeps its block:\n{cancelling}"
    );
    assert!(terminal.is_alive(), "Esc must never quit");
    terminal.wait_where("the turn settled", |frame| {
        !frame.contains("cancelling…")
            && !frame.contains("thinking")
            && !frame.contains("waiting for model")
    });

    terminal.type_draft(DRAFT);
    terminal.esc_esc_clear_draft(DRAFT);
    terminal.send(b"\x1a");
    terminal.wait_screen(DRAFT);
    terminal.send(b"\x19");
    terminal.wait_absent(DRAFT);
    terminal.send(b"\x1a");
    terminal.wait_screen(DRAFT);
    terminal.esc_esc_clear_draft(DRAFT);

    // The session is still usable after the cancel.
    terminal.send(b"after the cancel\r");
    terminal.wait_where("a turn after the cancel", |frame| {
        frame.contains("after the cancel") && frame.contains("the answer after the stall")
    });
    terminal.quit();
}

/// Waits until the caret column satisfies `predicate` and returns the caret.
fn wait_cursor_column(terminal: &mut Terminal, predicate: impl Fn(u16) -> bool) -> (u16, u16) {
    let deadline = Instant::now() + WAIT;
    loop {
        let cursor = terminal.cursor();
        if predicate(cursor.1) {
            return cursor;
        }
        assert!(
            Instant::now() < deadline,
            "the caret never moved as expected from {cursor:?}:\n{}",
            terminal.screen()
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// The composer's caret and kill chords, read back through the caret column
/// and the draft text; Alt+Enter, Ctrl+S and ↑ recall.
#[test]
fn composer_chords_move_the_caret_kill_text_and_recall_history() {
    let fixture = Fixture::new(&tool_then_answer("the composer answer"));
    let mut terminal = Terminal::spawn_ready(&fixture);

    terminal.type_draft("alpha beta gamma");
    let (row, end) = terminal.cursor();
    // Alt+B: one word left.
    terminal.send(b"\x1bb");
    let (_, after_alt_b) = wait_cursor_column(&mut terminal, |column| column < end);
    assert_eq!(
        usize::from(end - after_alt_b),
        "gamma".len(),
        "Alt+B is one word"
    );
    // Ctrl+Left: one more word left.
    terminal.send(b"\x1b[1;5D");
    let (_, after_ctrl_left) = wait_cursor_column(&mut terminal, |column| column < after_alt_b);
    assert_eq!(
        usize::from(after_alt_b - after_ctrl_left),
        "beta ".len(),
        "Ctrl+Left is one word"
    );
    // Alt+F and Ctrl+Right: back to the end.
    terminal.send(b"\x1bf");
    let (_, after_alt_f) = wait_cursor_column(&mut terminal, |column| column > after_ctrl_left);
    terminal.send(b"\x1b[1;5C");
    let (_, after_ctrl_right) = wait_cursor_column(&mut terminal, |column| column > after_alt_f);
    assert_eq!(
        after_ctrl_right, end,
        "Alt+F then Ctrl+Right returns to the end"
    );
    // Ctrl+A / Ctrl+E.
    terminal.send(b"\x01");
    let (_, start) = wait_cursor_column(&mut terminal, |column| column < after_ctrl_right);
    terminal.send(b"\x05");
    let (_, back_at_end) = wait_cursor_column(&mut terminal, |column| column > start);
    assert_eq!(back_at_end, end);
    let (row_after, _) = terminal.cursor();
    assert_eq!(row, row_after, "caret motion stays on the draft row");

    // Ctrl+W deletes the word before the caret; Alt+Backspace the next.
    terminal.send(b"\x17");
    terminal.wait_where("Ctrl+W deleted gamma", |frame| {
        frame.contains("alpha beta") && !frame.contains("gamma")
    });
    terminal.send(b"\x1b\x7f");
    terminal.wait_where("Alt+Backspace deleted beta", |frame| {
        frame.contains("❯ alpha") && !frame.contains("beta")
    });
    // Ctrl+A then Ctrl+K kills to the end; Ctrl+U on a fresh draft kills to
    // the start.
    terminal.send(b"\x01\x0b");
    terminal.wait_absent("alpha");
    terminal.type_draft("kill to the start");
    terminal.send(b"\x15");
    terminal.wait_absent("kill to the start");

    // Alt+Enter inserts a newline: the two halves land on different rows.
    terminal.type_draft("first half");
    terminal.send(b"\x1b\r");
    terminal.type_draft("second half");
    let two_rows = terminal.screen();
    assert!(
        row_of(&two_rows, "first half") < row_of(&two_rows, "second half"),
        "Alt+Enter must break the draft across rows:\n{two_rows}"
    );
    terminal.esc_esc_clear_draft("first half");

    // Ctrl+S sends now while idle; ↑ on an empty draft recalls it.
    terminal.type_draft("sent with ctrl s");
    terminal.send(b"\x13");
    let answered = terminal.wait_screen("the composer answer");
    assert_eq!(
        answered.matches("sent with ctrl s").count(),
        1,
        "the sent prompt is in the transcript once and the draft is empty:\n{answered}"
    );
    terminal.send(b"\x1b[A");
    let recalled = terminal.wait_where("↑ recalled the prior prompt", |frame| {
        frame.matches("sent with ctrl s").count() == 2
    });
    assert!(
        recalled.contains("❯ sent with ctrl s"),
        "the recalled prompt sits in the composer:\n{recalled}"
    );
    // Enter accepts the recalled entry into the draft; Enter again sends it.
    terminal.send(b"\r");
    std::thread::sleep(Duration::from_millis(150));
    terminal.send(b"\r");
    terminal.wait_where("the recalled prompt answered", |frame| {
        frame.matches("the composer answer").count() == 2
    });
    terminal.quit();
}

/// The queue chords while a turn is busy: Enter queues, Tab focuses, ↑/↓
/// select, Shift+↑ reorders, Ctrl+E edits, Ctrl+S steers, Ctrl+D drops (the
/// selected row when focused, else the last), and the queue drains once the
/// turn settles.
#[test]
fn queue_chords_select_reorder_edit_steer_and_drop_follow_ups() {
    let fixture = Fixture::new(&stalled_then_answers(QUEUE_STALL_MS));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"slow prompt\r");
    fixture.wait_for_provider_dispatch();
    terminal.wait_screen("waiting for model");

    // Enter while busy queues a follow-up.
    terminal.send(b"queued alpha\r");
    terminal.send(b"queued beta\r");
    terminal.send(b"queued gamma\r");
    let queued = terminal.wait_where("three queued follow-ups", |frame| {
        frame.contains("#1 queued alpha")
            && frame.contains("#2 queued beta")
            && frame.contains("#3 queued gamma")
    });
    assert!(
        queued.contains("3 queued"),
        "the HUD must count the queue:\n{queued}"
    );

    // Tab focuses the queue; ↓ selects the next row.
    terminal.send(b"\t");
    terminal.wait_where("the focused queue", |frame| {
        frame.contains("queue · focused") && frame.contains("❯#1 queued alpha")
    });
    terminal.send(b"\x1b[B");
    terminal.wait_screen("❯#2 queued beta");
    // Shift+↑ moves the selected row up.
    terminal.send(b"\x1b[1;2A");
    terminal.wait_where("beta moved above alpha", |frame| {
        frame.contains("❯#1 queued beta") && frame.contains("#2 queued alpha")
    });
    // ↑ on the first row stays; ↓ then Shift+↓ moves alpha below gamma.
    terminal.send(b"\x1b[B");
    terminal.wait_screen("❯#2 queued alpha");
    terminal.send(b"\x1b[1;2B");
    terminal.wait_where("alpha moved below gamma", |frame| {
        frame.contains("#2 queued gamma") && frame.contains("❯#3 queued alpha")
    });
    // Ctrl+E lifts the selected follow-up into the composer for editing.
    terminal.send(b"\x05");
    terminal.wait_where("the follow-up in the composer", |frame| {
        frame.contains("❯ queued alpha") && !frame.contains("queue · focused")
    });
    terminal.send(b"\x1b");
    terminal.wait_absent("❯ queued alpha");
    // Ctrl+S while busy steers.
    terminal.send(b"steer this way\x13");
    terminal.wait_where("the steer landed", |frame| {
        frame.contains("steer this way") && !frame.contains("◇ steer")
    });
    // Ctrl+D drops the selected row of the focused queue, and nothing else.
    terminal.send(b"\t");
    terminal.wait_screen("queue · focused");
    terminal.send(b"\x1b[B\x1b[B");
    terminal.wait_screen("❯#3 queued alpha");
    terminal.send(b"\x04");
    terminal.wait_where("Ctrl+D dropped the last", |frame| {
        !frame.contains("queued alpha") && frame.contains("2 queued")
    });
    terminal.send(b"\x1b[A\x1b[A");
    terminal.wait_screen("❯#1 queued beta");
    terminal.send(b"\x04");
    terminal.wait_where("Ctrl+D dropped the selected row", |frame| {
        !frame.contains("queued beta") && frame.contains("#1 queued gamma")
    });
    terminal.send(b"\x1b");
    terminal.wait_absent("queue · focused");

    // The stall settles; the remaining follow-up drains and is answered.
    terminal.wait_where("the queued follow-up drained", |frame| {
        frame.contains("❯ queued gamma") && frame.matches("the answer after the stall").count() >= 1
    });
    terminal.wait_where("the follow-up answered", |frame| {
        frame.matches("the answer after the stall").count() >= 2
            || frame.contains("the stalled answer")
    });
    terminal.quit();
}

#[test]
fn a_queued_prompt_sent_now_shows_its_steer_row_while_the_turn_runs() {
    assert_queued_send_now_shows_live_steer(b"\r");
}

#[test]
fn a_focused_queue_ctrl_s_shows_its_steer_row_while_the_turn_runs() {
    assert_queued_send_now_shows_live_steer(b"\x13");
}

fn assert_queued_send_now_shows_live_steer(chord: &[u8]) {
    let fixture = Fixture::new(&stalled_then_answers(QUEUE_STALL_MS));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"slow prompt\r");
    fixture.wait_for_provider_dispatch();
    terminal.wait_screen("waiting for model");
    terminal.send(b"queued alpha\r");
    terminal.wait_screen("#1 queued alpha");
    terminal.send(b"\t");
    terminal.wait_screen("queue · focused");
    terminal.send(chord);
    let live = terminal.wait_where("the live send-now steer row", |frame| {
        frame
            .lines()
            .any(|line| line.contains("queued alpha") && line.contains("steer"))
            && !frame.contains("#1 queued alpha")
    });
    assert!(
        live.contains("waiting for model")
            && !live.contains("the stalled answer")
            && !live.contains("the answer after the stall"),
        "the steer must be visible before the running turn answers:\n{live}"
    );
    let settled = terminal.wait_where("the steered turn settled", |frame| {
        frame.contains("the answer after the stall") && !frame.contains("waiting for model")
    });
    assert_eq!(settled.matches("queued alpha").count(), 1, "{settled}");
    assert!(
        row_of(&settled, "queued alpha") < row_of(&settled, "the answer after the stall"),
        "the single steer row must precede its answer:\n{settled}"
    );
    terminal.quit();
}

/// Two send-nows in one turn: the first steer's delivery is settled when it
/// is acknowledged, so the second is admitted rather than refused as an
/// unsaved queue change.
#[test]
fn two_send_nows_in_one_turn_both_steer() {
    let fixture = Fixture::new(&stalled_then_answers(QUEUE_STALL_MS));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"slow prompt\r");
    fixture.wait_for_provider_dispatch();
    terminal.wait_screen("waiting for model");
    terminal.send(b"queued alpha\r");
    terminal.send(b"queued beta\r");
    terminal.wait_where("two queued follow-ups", |frame| {
        frame.contains("#1 queued alpha") && frame.contains("#2 queued beta")
    });
    // Empty Enter while busy sends the queue head now, two seconds after
    // the last typing key or Enter (a doubled Enter sends nothing more).
    std::thread::sleep(Duration::from_millis(2_100));
    terminal.send(b"\r");
    terminal.wait_where("the first send-now steered", |frame| {
        frame.contains("#1 queued beta") && !frame.contains("#2 queued")
    });
    std::thread::sleep(Duration::from_millis(2_100));
    terminal.send(b"\r");
    let frame = terminal.wait_where("the second send-now steered", |frame| {
        !frame.contains("#1 queued") || frame.contains("not saved")
    });
    assert!(
        !frame.contains("not saved") && !frame.contains("#1 queued"),
        "the second send-now must not be refused:\n{frame}"
    );
    terminal.quit();
}

/// Menu chords: tabs by ←/→ and Tab/Shift+Tab, ↑/↓ over rows,
/// Space, `d` then Enter or Esc, `/` search with printable keys and Backspace,
/// `PageUp`/`PageDown` and Home/End over the Keybinds sheet, `y` copy, Esc.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one PTY session walks every Menu chord"
)]
fn control_center_chords_switch_tabs_edit_rows_search_scroll_and_copy() {
    let fixture = Fixture::new(&tool_then_answer("unused"));
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"/settings\r");
    let settings = terminal.wait_where("the Settings tab", |frame| {
        frame.contains("Menu") && frame.contains("/ search settings")
    });
    assert!(
        settings.contains("Theme") && settings.contains("default-dark"),
        "the Settings tab names the Theme row:\n{settings}"
    );

    terminal.focus_control_row("Auto compaction");
    terminal.send(b" ");
    terminal.wait_where("Auto compaction off", |frame| {
        frame.contains("now off · default on")
    });
    // `d` opens the confirmation pop-up; Esc keeps; `d` then Enter resets.
    terminal.send(b"d");
    terminal.wait_screen("Reset Auto compaction?");
    terminal.send(b"\x1b");
    terminal.wait_where("Esc kept the value", |frame| {
        !frame.contains("Reset Auto compaction?") && frame.contains("now off · default on")
    });
    terminal.send(b"d");
    terminal.wait_screen("Reset Auto compaction?");
    terminal.send(b"\r");
    terminal.wait_where("Enter reset the value", |frame| {
        !frame.contains("Reset Auto compaction?") && frame.contains("now on · default on")
    });
    // `/` searches; printable keys filter; Backspace edits the filter.
    terminal.send(b"/mou");
    terminal.wait_where("the search narrowed to Mouse capture", |frame| {
        frame.contains("/ mou") && frame.contains("Mouse capture") && !frame.contains("▸ Theme")
    });
    terminal.send(b"\x7f\x7f\x7f");
    terminal.wait_where("Backspace cleared the search", |frame| {
        frame.contains("/ search settings") && frame.contains("Theme")
    });
    terminal.focus_control_row("Theme");
    terminal.send(b"\r");
    let chooser = terminal.wait_where("the Theme chooser", |frame| {
        frame.contains("Default Dark") && frame.contains("Default Light")
    });
    assert!(chooser.contains("Menu"), "{chooser}");
    terminal.send(b"\x1b");
    terminal.wait_where("Esc closed the chooser", |frame| {
        frame.contains("Menu") && frame.contains("▾ Theme")
    });

    // → moves to Agents. Down walks the profile list; typing does not filter it.
    terminal.send(b"\x1b[C");
    terminal.wait_screen("choose when it applies");
    terminal.wait_screen("gpt-6-astra");
    terminal.send(b"\x1b[B\x1b[B");
    terminal.wait_screen("grok-4.7");
    // ← back to Settings; Tab forward to Agents and Providers; Shift+Tab back.
    terminal.send(b"\x1b[D");
    terminal.wait_screen("/ search settings");
    terminal.send(b"\t");
    terminal.wait_screen("choose when it applies");
    terminal.send(b"\t");
    terminal.wait_where("the Providers tab", |frame| {
        !frame.contains("choose when it applies") && frame.contains("plak login")
    });
    terminal.send(b"\x1b[Z");
    terminal.wait_screen("choose when it applies");

    // → to Usage, where `y` copies the tab through OSC 52.
    terminal.send(b"\x1b[C\x1b[C");
    terminal.wait_where("the Usage tab", |frame| {
        frame.contains("Menu")
            && !frame.contains("choose when it applies")
            && !frame.contains("plak login")
    });
    let before = terminal.raw_len();
    terminal.send(b"y");
    let deadline = Instant::now() + WAIT;
    while !terminal
        .raw_since(before)
        .windows(4)
        .any(|w| w == b"\x1b]52")
    {
        assert!(
            Instant::now() < deadline,
            "y on the Usage tab never wrote an OSC 52 clipboard sequence"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // → → → to Keybinds; Enter opens the full sheet before scrolling.
    terminal.send(b"\x1b[C\x1b[C\x1b[C");
    terminal.wait_screen("While typing");
    terminal.send(b"\r");
    let sheet = terminal.wait_screen("All keybinds");
    assert!(sheet.contains("global"), "{sheet}");
    terminal.send(b"\x1b[6~");
    terminal.wait_where("PageDown scrolled the sheet", |frame| {
        !frame.contains("quit the TUI") && frame.contains("Menu")
    });
    terminal.send(b"\x1b[F");
    terminal.wait_where("End reached the bottom", |frame| {
        frame.contains("as does a click outside it")
    });
    terminal.send(b"\x1b[H");
    terminal.wait_screen("quit the TUI");
    terminal.send(b"\x1b[6~");
    terminal.wait_absent("quit the TUI");
    terminal.send(b"\x1b[5~");
    terminal.wait_screen("quit the TUI");

    terminal.send(b"\x1b");
    terminal.wait_screen("While typing");
    terminal.send(b"\x1b");
    terminal.wait_absent("Menu");
    terminal.quit();
}

/// Help-overlay chords: ↑/↓, PageUp/PageDown, Home/End, and `q`, Enter and
/// Esc to close. A 20-row pane makes the overlay scroll.
/// The Agents tab through the real binary: the product lists the profiles
/// and says why a file was left out, a filter that hides every row offers
/// nothing on Enter, `/agents <alias>` resolves like the list, and a choice
/// lands as the product's typed answer.
#[test]
fn the_agents_tab_lists_filters_and_applies_profiles_through_the_product() {
    let fixture = Fixture::new(&tool_then_answer("unused"));
    std::fs::create_dir_all(fixture.home().join("profiles")).expect("profiles");
    write_private(
        &fixture.home().join("profiles").join("codex-default.toml"),
        "root = \"lead\"\nworking_set = [\"lead\"]\n",
    );
    let config_before = std::fs::read_to_string(fixture.home().join("config.toml")).unwrap();
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"/agents\r");
    terminal.wait_where("the product's profile list", |frame| {
        frame.contains("codex-default")
            && frame.contains("grok-default")
            && frame.contains("codex-default.toml is ignored")
    });
    terminal.send(b"zzz");
    terminal.wait_screen("No matching profiles");
    terminal.send(b"\r");
    std::thread::sleep(Duration::from_millis(200));
    let frame = terminal.screen();
    assert!(
        !frame.contains("Next session"),
        "Enter on a hidden row offered it:\n{frame}"
    );
    terminal.send(b"\x1b");
    terminal.wait_where("Esc cleared the filter", |frame| {
        frame.contains("type to filter") && frame.contains("claude-default")
    });
    terminal.send(b"\x1b");
    terminal.wait_ready();
    terminal.send(b"/agents grok\r");
    terminal.wait_where("the grok-default dialog", |frame| {
        frame.contains("Next session") && frame.contains("Apply profile grok-default?")
    });
    // Next session keeps the tab open; the answer lands in the transcript.
    // Focus starts on Fork now, so one → reaches Next session.
    terminal.send(b"\x1b[C");
    terminal.send(b"\r");
    std::thread::sleep(Duration::from_millis(200));
    terminal.send(b"\x1b");
    let answered = terminal.wait_where("the product's answer", |frame| {
        frame.contains("profile grok-default")
    });
    // No grok credential exists offline: the product refuses, and a refused
    // choice writes nothing.
    assert!(answered.contains("grok is not ready"), "{answered}");
    assert_eq!(
        std::fs::read_to_string(fixture.home().join("config.toml")).unwrap(),
        config_before
    );
    assert!(
        !fixture
            .home()
            .join("agents")
            .join("grok-default-root.toml")
            .exists()
    );
    terminal.quit();
}

#[test]
fn help_overlay_chords_scroll_and_close() {
    let fixture = Fixture::new(&tool_then_answer("unused"));
    let mut terminal = Terminal::spawn_sized(&fixture, Launch::Fresh, 20, TUI_COLS);
    terminal.wait_ready();
    terminal.send(b"/help\r");
    let help = terminal.wait_screen("Show all commands");
    assert!(help.contains("commands:"), "{help}");
    terminal.send(b"\x1b[B");
    terminal.wait_where("↓ scrolled one line", |frame| {
        !frame.contains("commands:") && frame.contains("Show all commands")
    });
    terminal.send(b"\x1b[A");
    terminal.wait_screen("commands:");
    terminal.send(b"\x1b[6~");
    terminal.wait_where("PageDown scrolled a screen", |frame| {
        !frame.contains("Show all commands") && frame.contains("Exit Plak")
    });
    terminal.send(b"\x1b[F");
    terminal.wait_where("End reached the bottom", |frame| {
        frame.contains("as does a click outside it")
    });
    terminal.send(b"\x1b[H");
    terminal.wait_screen("commands:");
    terminal.send(b"\x1b[6~");
    terminal.wait_absent("commands:");
    terminal.send(b"\x1b[5~");
    terminal.wait_screen("commands:");
    terminal.send(b"q");
    terminal.wait_absent("Show all commands");

    terminal.send(b"/help\r");
    terminal.wait_screen("Show all commands");
    terminal.send(b"\r");
    terminal.wait_absent("Show all commands");
    terminal.send(b"/help\r");
    terminal.wait_screen("Show all commands");
    terminal.send(b"\x1b");
    terminal.wait_absent("Show all commands");
    terminal.quit();
}

// ---------------------------------------------------------------------------
// Case 8: quit and reopen through the session picker
// ---------------------------------------------------------------------------

/// Two sessions written by two earlier terminals; the launch picker lists
/// them, ↑/↓ and j/k move the selection, Esc and q decline, Enter reopens the
/// chosen one with its transcript, and the in-session `/resume` picker opens
/// the same list.
#[test]
fn the_launch_picker_lists_earlier_sessions_and_reopens_the_chosen_one() {
    let fixture = Fixture::new(&tool_then_answer("the picker answer"));
    for prompt in ["alpha session prompt", "beta session prompt"] {
        let mut terminal = Terminal::spawn_ready(&fixture);
        terminal.send(format!("{prompt}\r").as_bytes());
        terminal.wait_screen("the picker answer");
        terminal.quit();
    }
    assert_eq!(fixture.session_ids().len(), 2);

    let mut picker = Terminal::spawn(&fixture, Launch::Picker);
    // Rows clip the opening prompt; the detail panel under the list names the
    // selected session's prompt in full, so selection is read from there.
    let panel_shows = |frame: &str, prompt: &str| frame.contains(&format!("│ {prompt}"));
    let listing = picker.wait_where("the picker lists both sessions", |frame| {
        frame.contains("Resume session")
            && frame.contains("beta session")
            && frame.contains("alpha session")
            && (panel_shows(frame, "alpha session prompt")
                || panel_shows(frame, "beta session prompt"))
    });
    assert!(
        listing.matches("offline-loopback-v1").count() >= 2,
        "both rows name the model:\n{listing}"
    );
    let started_on_alpha = panel_shows(&listing, "alpha session prompt");
    picker.send(b"\x1b[B");
    picker.wait_where("↓ moved the selection", |frame| {
        if started_on_alpha {
            panel_shows(frame, "beta session prompt")
        } else {
            panel_shows(frame, "alpha session prompt")
        }
    });
    picker.send(b"\x1b[A");
    picker.wait_where("↑ moved back", |frame| {
        if started_on_alpha {
            panel_shows(frame, "alpha session prompt")
        } else {
            panel_shows(frame, "beta session prompt")
        }
    });
    picker.send(b"\x1b[B");
    picker.wait_where("↓ moved down", |frame| {
        if started_on_alpha {
            panel_shows(frame, "beta session prompt")
        } else {
            panel_shows(frame, "alpha session prompt")
        }
    });
    picker.send(b"\x1b[A");
    picker.wait_where("↑ moved up", |frame| {
        if started_on_alpha {
            panel_shows(frame, "alpha session prompt")
        } else {
            panel_shows(frame, "beta session prompt")
        }
    });
    // Esc declines the picker; the shell attaches the workspace default.
    picker.send(b"\x1b");
    picker.wait_where("the declined picker", |frame| {
        !frame.contains("Resume session") && frame.contains("/ commands") && frame.contains("%/")
    });
    picker.send(b"\x11");
    assert!(picker.finish().success());

    // The second row is the older session; Enter reopens it with its own
    // transcript and nothing of the other.
    let mut reopened = Terminal::spawn(&fixture, Launch::Picker);
    let opened = reopened.wait_screen("Resume session");
    if !panel_shows(&opened, "alpha session prompt") {
        reopened.send(b"\x1b[B");
        reopened.wait_where("the older session selected", |frame| {
            panel_shows(frame, "alpha session prompt")
        });
    }
    // The picker's `Enter` acts once it has been open for 600 ms.
    std::thread::sleep(Duration::from_millis(700));
    reopened.send(b"\r");
    let restored = reopened.wait_where("the older session restored", |frame| {
        !frame.contains("Resume session")
            && frame.contains("❯ alpha session prompt")
            && frame.contains("the picker answer")
            && frame.contains("/ commands")
    });
    assert!(
        !restored.contains("beta session prompt"),
        "the other session's transcript leaked in:\n{restored}"
    );
    reopened.send(b"prompt on the reopened session\r");
    reopened.wait_where("the reopened session answered", |frame| {
        frame.contains("prompt on the reopened session")
            && frame.matches("the picker answer").count() >= 2
    });

    // The in-session picker: `/resume` lists the sessions, `Esc` declines
    // and keeps the current one.
    reopened.send(b"/resume\r");
    reopened.wait_where("the in-session picker", |frame| {
        frame.contains("Resume session") && frame.contains("beta session")
    });
    reopened.send(b"\x1b");
    let kept = reopened.wait_absent("Resume session");
    assert!(
        kept.contains("prompt on the reopened session"),
        "Esc must keep the current session:\n{kept}"
    );
    reopened.quit();
}

// ---------------------------------------------------------------------------
// Session carry-forward
// ---------------------------------------------------------------------------

/// `/reload` onto the same state key switches executables and resumes the
/// exact session id without creating an export or replacement session.
#[test]
fn a_reload_onto_the_same_state_key_resumes_exactly() {
    let fixture = Fixture::new(&tool_then_answer("same-key answer"));
    let key = Command::new(env!("CARGO_BIN_EXE_plak"))
        .args(["version", "--state-key"])
        .current_dir(fixture.workspace())
        .env("HOME", fixture.root.join("fake-home"))
        .env("TMPDIR", fixture.root.join("tmp"))
        .env("PLAK_HOME", fixture.home())
        .output()
        .expect("state key command");
    assert!(key.status.success());
    let key = String::from_utf8(key.stdout).expect("utf-8 state key");
    let fake_bin = fixture.root.join("fake-bin-same-key");
    std::fs::create_dir_all(&fake_bin).expect("fake bin");
    let args = fixture.root.join("same-key-reload-args");
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&fake_bin));
    terminal.wait_ready();
    write_script_file(
        &fake_bin.join("plak"),
        &format!(
            "#!/bin/sh\n\
             if [ \"$1 $2\" = 'version --state-key' ]; then echo '{}'; exit 0; fi\n\
             printf '%s\\n' \"$@\" > '{}'\n\
             exit 0\n",
            key.trim(),
            args.display()
        ),
    );
    terminal.send(b"same key prompt\r");
    terminal.wait_screen("same-key answer");
    let session = fixture.session_ids()[0].clone();
    let reload_started = Instant::now();
    let raw_before_reload = terminal.raw_len();
    let deadline = reload_started + Duration::from_millis(1200);
    while terminal.is_alive() {
        assert!(
            Instant::now() < deadline,
            "same-key reload exceeded the exit budget"
        );
        terminal.send_until_exit(b"/reload\r");
        std::thread::sleep(Duration::from_millis(5));
    }
    let reload_raw = terminal.raw_since(raw_before_reload);
    let restore = reload_raw
        .windows(8)
        .position(|window| window == b"\x1b[?1049l")
        .expect("reload restores alternate screen");
    let notice = reload_raw
        .windows("reloading…".len())
        .position(|window| window == "reloading…".as_bytes())
        .expect("reload notice");
    assert!(restore < notice, "restore precedes reload notice");
    eprintln!("TUI reload: {}ms", reload_started.elapsed().as_millis());
    assert!(terminal.finish().success());
    assert_eq!(
        std::fs::read_to_string(args).expect("reload args"),
        format!("--resume\n{session}\n")
    );
    assert!(!fixture.home().join("carry").exists());
    assert_eq!(fixture.session_ids(), [session]);
}

/// `/reload` onto an installed build that reports another state key exports
/// the session it is leaving (after a turn, from the record the host last
/// committed) and starts that build with `--import-resume <file>`.
#[test]
fn a_reload_onto_another_state_key_hands_the_new_build_an_export() {
    let fixture = Fixture::new(&tool_then_answer("the answer before reload"));
    let fake_bin = fixture.root.join("fake-bin");
    std::fs::create_dir_all(&fake_bin).expect("fake bin");
    let mut terminal = Terminal::spawn(&fixture, Launch::OnPath(&fake_bin));
    terminal.wait_ready();
    terminal.send(b"carry this conversation\r");
    terminal.wait_screen("the answer before reload");

    let args = fixture.root.join("reload-args");
    let copy = fixture.root.join("carried.json");
    write_script_file(
        &fake_bin.join("plak"),
        &format!(
            "#!/bin/sh\n\
             if [ \"$1 $2\" = 'version --state-key' ]; then echo r0.e0.p0.t0; exit 0; fi\n\
             printf '%s\\n' \"$@\" > '{}'\n\
             [ \"$1\" = --import-resume ] && cp \"$2\" '{}'\n\
             exit 0\n",
            args.display(),
            copy.display()
        ),
    );
    // The turn may still be settling; a refused reload leaves the TUI up.
    let deadline = Instant::now() + WAIT;
    while terminal.is_alive() {
        assert!(
            Instant::now() < deadline,
            "reload never left\n{}",
            terminal.screen()
        );
        terminal.send_until_exit(b"/reload\r");
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(terminal.finish().success());

    let args = std::fs::read_to_string(&args).expect("the new build ran");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args.len(), 2, "{args:?}");
    assert_eq!(args[0], "--import-resume");
    let carry_file = Path::new(args[1]);
    assert!(
        carry_file.starts_with(fixture.home().join("carry")),
        "{args:?}"
    );
    assert_eq!(
        std::fs::metadata(carry_file)
            .expect("carry file")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let document: Value =
        serde_json::from_slice(&std::fs::read(&copy).expect("copied export")).expect("json");
    assert_eq!(document["format"], "plak-session-export");
    assert_eq!(
        document["source"]["session_id"],
        fixture.session_ids()[0].as_str()
    );
    let checkpoint = document["checkpoint"].to_string();
    assert!(
        checkpoint.contains("carry this conversation"),
        "{checkpoint}"
    );
    assert!(
        checkpoint.contains("the answer before reload"),
        "{checkpoint}"
    );
    assert!(
        document["archive"]
            .to_string()
            .contains("the answer before reload")
    );
}

/// The new build's half of a reload: `--import-resume` opens the carried
/// session with its note and spends the file.
#[test]
fn import_resume_opens_the_carried_session_and_deletes_the_file() {
    let fixture = Fixture::new(&tool_then_answer("an answer worth carrying"));
    let old = fixture.session_with_one_turn("first prompt", "an answer worth carrying");
    let path = fixture.export(&old, "carry.json");
    let path_text = path.to_str().expect("utf-8 path").to_owned();
    let mut terminal = Terminal::spawn(&fixture, Launch::Args(&["--import-resume", &path_text]));
    terminal.wait_screen("linked from");
    terminal.wait_screen("an answer worth carrying");
    assert!(!path.exists(), "a spent carry file is deleted");
    terminal.quit();
    let ids: std::collections::BTreeSet<String> = fixture.session_ids().into_iter().collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
}

/// `plak --resume <id>` of a session another state key owns is exported by
/// that key's kept maintainer snapshot and imported here; resuming the old id
/// again reopens the same carried session instead of copying it twice.
#[test]
fn resuming_another_builds_session_carries_it_once_through_its_snapshot() {
    let fixture = Fixture::new(&tool_then_answer("an answer from the old build"));
    let old = fixture.session_with_one_turn("old build prompt", "an answer from the old build");
    let export = fixture.export(&old, "stash.json");
    let mut document: Value =
        serde_json::from_slice(&std::fs::read(&export).expect("export")).expect("json");
    document["source"]["state_key"] = json!("r0.e0.p0.t0");
    std::fs::write(&export, serde_json::to_vec(&document).expect("json")).expect("stash");

    // This build's state becomes another build's.
    let state = fixture.home().join("state");
    let current = std::fs::read_dir(&state)
        .expect("state")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .expect("this build's state dir");
    std::fs::rename(&current, state.join("r0.e0.p0.t0")).expect("move state aside");
    let snapshot = fixture
        .root
        .join("fake-home/.local/lib/plak/maintainer")
        .join("0".repeat(64));
    std::fs::create_dir_all(&snapshot).expect("snapshot");
    std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o755))
        .expect("snapshot mode");
    std::fs::write(snapshot.join("state-key"), "r0.e0.p0.t0\n").expect("manifest");
    write_script_file(
        &snapshot.join("plak"),
        &format!(
            "#!/bin/sh\n\
             [ \"$1 $2 $3\" = 'session export {old}' ] && cp '{}' \"$5\" && exit 0\n\
             exit 3\n",
            export.display()
        ),
    );

    let mut terminal = Terminal::spawn(&fixture, Launch::Resume(&old));
    terminal.wait_screen("linked from");
    terminal.wait_screen("an answer from the old build");
    terminal.quit();
    // The listing names the old id too, in the carried session's link.
    let carried: std::collections::BTreeSet<String> = fixture.session_ids().into_iter().collect();
    assert_eq!(carried.len(), 2, "{carried:?}");
    assert!(carried.contains(&old), "{carried:?}");

    let mut terminal = Terminal::spawn(&fixture, Launch::Resume(&old));
    terminal.wait_screen("already carried forward");
    terminal.quit();
    let again: std::collections::BTreeSet<String> = fixture.session_ids().into_iter().collect();
    assert_eq!(again, carried);
}

#[test]
fn tui_exit_bound_idle_and_stalled_turn() {
    for stalled in [false, true] {
        let mut script = stalled_then_answers(30_000);
        // A deaf provider cannot settle cancellation. Exit must not join it.
        script["stall_ignores_cancellation"] = json!(true);
        let fixture = Fixture::new(&script);
        let mut terminal = Terminal::spawn(&fixture, Launch::Fresh);
        terminal.wait_ready();
        if stalled {
            terminal.send(b"stall until quit\r");
            fixture.wait_for_provider_dispatch();
            terminal.wait_screen("waiting for model");
        }
        terminal.send(b"\x11");
        if stalled {
            // Mid-turn quit asks first; the bound runs from `[ Quit ]`.
            terminal.wait_screen("Quit plak?");
        }
        let started = Instant::now();
        if stalled {
            terminal.send(b"\r");
        }
        while terminal.is_alive() {
            assert!(
                started.elapsed() < Duration::from_millis(1200),
                "quit exceeded its 1s bound plus PTY scheduling slack"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let elapsed = started.elapsed();
        assert!(terminal.finish().success());
        eprintln!("TUI quit stalled={stalled}: {}ms", elapsed.as_millis());
    }
}

fn product_event_count(database: &rusqlite::Connection, kind: &str) -> i64 {
    database
        .query_row(
            "SELECT count(*) FROM product_events WHERE kind = ?1",
            [kind],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn tui_local_launch_budget_new_and_resume_with_network_faults() {
    let mut measurements = [Vec::new(), Vec::new()];
    for sample in 1..=3 {
        let fixture = Fixture::new(&tool_then_answer("warm startup answer"));
        let mut session = None;
        for resumed in [false, true] {
            let started = Instant::now();
            let mut terminal = Terminal::spawn(
                &fixture,
                Launch::Faulted {
                    resume: session.as_deref(),
                    faults: "fault=stall,seconds=30",
                },
            );
            // End of synchronized output measures drawing, not host-ready work.
            while !terminal
                .raw_since(0)
                .windows(8)
                .any(|bytes| bytes == b"\x1b[?2026l")
            {
                assert!(started.elapsed() < WAIT, "local first frame never appeared");
                std::thread::sleep(Duration::from_millis(2));
            }
            let frame_elapsed = started.elapsed();
            let typing_started = Instant::now();
            terminal.send(b"local launch draft");
            while !terminal.screen().contains("local launch draft") {
                assert!(
                    typing_started.elapsed() < WAIT,
                    "visible key echo never appeared"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            let typed_elapsed = typing_started.elapsed();
            terminal.wait_ready();
            // Process-start telemetry excludes this test's PTY/thread setup.
            let database = rusqlite::Connection::open(
                plak::home::paths::PlakPaths::explicit(fixture.home()).telemetry_database(),
            )
            .unwrap();
            let [local_frame_ns, local_ready_ns] =
                local_launch_timings(&database, resumed, started);
            eprintln!(
                "TUI launch sample={sample} resumed={resumed}: local_first_frame={}ns local_ready_to_type={}ns spawn_to_frame={}ms key_echo={}ms",
                local_frame_ns,
                local_ready_ns,
                frame_elapsed.as_millis(),
                typed_elapsed.as_millis(),
            );
            measurements[usize::from(resumed)].push((
                local_frame_ns,
                local_ready_ns,
                typed_elapsed,
            ));
            if !resumed {
                // Publish a real session before measuring its next-process resume.
                terminal.send(b"\x15warm startup prompt\r");
                terminal.wait_screen("warm startup answer");
            }
            #[cfg(feature = "maintainer")]
            assert_startup_attribution(&mut terminal, &fixture, resumed);
            terminal.send(b"\x11");
            assert!(terminal.finish().success());
            session = Some(fixture.session_ids()[0].clone());
        }
        assert_local_launch_telemetry(&fixture);
    }
    for (resumed, samples) in measurements.into_iter().enumerate() {
        let frame = samples.iter().map(|(frame, _, _)| *frame).min().unwrap();
        let ready = samples.iter().map(|(_, ready, _)| *ready).min().unwrap();
        let echo = samples
            .iter()
            .map(|(frame, _, echo)| u128::try_from(*frame).unwrap() + echo.as_nanos())
            .min()
            .unwrap();
        assert!(
            frame < 200_000_000,
            "scenario {resumed}: best process-local first frame exceeded 200ms: {frame}ns"
        );
        assert!(
            ready < 500_000_000,
            "scenario {resumed}: best process-local ready to type exceeded 500ms: {ready}ns"
        );
        assert!(
            echo < 500_000_000,
            "scenario {resumed}: best process-local frame plus visible key echo exceeded 500ms: {echo}ns"
        );
    }
}

#[cfg(feature = "maintainer")]
fn assert_startup_attribution(terminal: &mut Terminal, fixture: &Fixture, resumed: bool) {
    let log_path = fixture.home().join("logs/plak.log");
    let suffix = if resumed {
        "command=resume"
    } else {
        "command=tui"
    };
    terminal.wait_where("startup build attribution", |_| {
        std::fs::read_to_string(&log_path).is_ok_and(|log| {
            log.lines()
                .any(|line| line.contains("INFO plak.start:") && line.ends_with(suffix))
        })
    });
    let log = std::fs::read_to_string(log_path).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("INFO plak.start:") && line.ends_with(suffix))
            .count(),
        1,
        "one attribution line for this TUI launch"
    );
}

fn assert_local_launch_telemetry(fixture: &Fixture) {
    let database = rusqlite::Connection::open(
        plak::home::paths::PlakPaths::explicit(fixture.home()).telemetry_database(),
    )
    .unwrap();
    for kind in [
        "tui_process_start",
        "tui_first_frame_local",
        "tui_ready_to_type_local",
    ] {
        assert_eq!(
            product_event_count(&database, kind),
            2,
            "new and resumed telemetry for {kind}"
        );
    }
    for kind in ["tui_key_to_restore", "tui_restore_to_exit"] {
        assert_eq!(
            product_event_count(&database, kind),
            1,
            "the next open imports the earlier exit's {kind}"
        );
    }
}

fn local_launch_timings(
    database: &rusqlite::Connection,
    resumed: bool,
    started: Instant,
) -> [i64; 2] {
    use rusqlite::OptionalExtension as _;
    loop {
        let values = ["tui_first_frame_local", "tui_ready_to_type_local"].map(|kind| {
            database
                .query_row(
                    "SELECT elapsed_ns FROM product_events WHERE kind = ?1
                     AND status = ?2 ORDER BY at DESC LIMIT 1",
                    [kind, if resumed { "resume" } else { "new" }],
                    |row| row.get(0),
                )
                .optional()
                .unwrap()
        });
        if let [Some(frame), Some(ready)] = values {
            return [frame, ready];
        }
        assert!(
            started.elapsed() < WAIT,
            "local launch timings were not emitted"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
