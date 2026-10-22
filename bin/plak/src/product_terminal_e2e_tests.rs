//! Real-PTY offline qualification of Plak's private product composition.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private process-level test harness"
)]

use std::collections::HashSet;
use std::error::Error;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plakous::agent::ReasoningEffort;
use plakous::application::ApplicationExecutor;
use plakous::sessions::{
    AgentSessions, OpenSessionOutcome, OpenSessionRequest, SessionBackend, SessionOperationId,
};
use plakous::subscriptions::NativeProvider;
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{
    BoundaryController, exited_child_process_runtime_binding, runtime_binding,
    runtime_binding_for_account, runtime_binding_for_account_with_identity_namespace,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::commands::host_config::TuiAttacher;
use crate::product_host::{HostConfig, RuntimeRole, ScriptedRuntimeFactory};
use crate::product_paths::PlakPaths;
use crate::product_sessions::{self, SessionRecord, TranscriptRow};
use crate::test_support::latency_budget;
use crate::tui_launch::launch_tui_with_attacher;
use crate::{HostedAgentChannels, ProcessOutcome};

const CHILD_TEST: &str = "product_terminal_e2e_tests::terminal_e2e_child";
const COLS: u16 = 100;
const ROWS: u16 = 30;
// This test launches a nested Rust test process, opens two SQLite stores, and
// performs a complete provider/tool/provider turn. Keep a bounded but
// contention-tolerant wall limit; semantic completion is asserted separately.
const WAIT: Duration = Duration::from_secs(45);

struct OfflineAttacher {
    evidence: PathBuf,
    process_lifecycle: bool,
    profile_fork: bool,
    long_work_release: Option<PathBuf>,
    /// Park the first provider *response stream* until this file appears.
    ///
    /// Gating the dispatch itself (`long_work_release`) parks the model before
    /// it ever yields a stream, which is an unjoinable operation: a cancel there
    /// is correctly reported as recovery-required, not as an interrupt. Gating
    /// the outcome instead leaves a real streaming turn that Esc and Ctrl+C can
    /// cancel through the ordinary §6.1 interrupted path.
    stream_gate_release: Option<PathBuf>,
    /// Park the mutating loopback effect *inside its dispatch* until this file
    /// appears, so the TUI sits on a running tool that a cancel must join.
    effect_gate_release: Option<PathBuf>,
    /// Refuse this many attach attempts before the injected runtime attaches.
    ///
    /// A `plak resume` launch on a workspace whose default cannot be resolved
    /// parks on a not-ready shell with the picker over it, which is the exact
    /// state that stranded the operator. One refusal reproduces it and
    /// lets the next attempt succeed; a refusal count no round can exhaust
    /// reproduces the workspace that has no attachable default at all.
    remaining_attach_refusals: AtomicUsize,
}

/// Which loopback boundary a real-terminal test parks the turn on.
#[derive(Clone, Copy)]
enum TurnGate<'a> {
    /// The first provider response stream.
    Stream(&'a Path),
    /// The running mutating loopback effect.
    Effect(&'a Path),
}

impl TuiAttacher for OfflineAttacher {
    fn attach(
        &self,
        paths: &PlakPaths,
        state_roots: &crate::product_agent::ProductStateRoots,
        session: SessionRecord,
        _account: Option<&str>,
        _model_id: Option<&str>,
        effort: ReasoningEffort,
        effort_explicit: bool,
        cwd: &Path,
        ready_catalog: &plakous_host_protocol::views::ReadyCatalog,
        resume_publication: Option<Arc<AtomicBool>>,
    ) -> Result<HostedAgentChannels, String> {
        if self
            .remaining_attach_refusals
            .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err("injected attach refusal".to_owned());
        }
        let controller = if let Some(release) = &self.long_work_release {
            BoundaryController::quiet_gated_from_evidence_path(
                CrashBoundary::ProviderDispatchOne,
                &self.evidence,
                release,
            )
        } else if let Some(release) = &self.stream_gate_release {
            BoundaryController::quiet_gated_from_evidence_path(
                CrashBoundary::ProviderOutcomeOne,
                &self.evidence,
                release,
            )
        } else if let Some(release) = &self.effect_gate_release {
            BoundaryController::quiet_gated_from_evidence_path(
                CrashBoundary::EffectDispatch,
                &self.evidence,
                release,
            )
        } else {
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &self.evidence)
        }
        .map_err(|error| error.to_string())?;
        let long_work = self.long_work_release.is_some();
        let profile_fork = self.profile_fork;
        let target = if profile_fork {
            runtime_binding_for_account(cwd, controller.clone(), b"tui-target-claude")
        } else if long_work {
            runtime_binding_for_account(cwd, controller.clone(), b"long-work-target")
        } else if self.process_lifecycle {
            exited_child_process_runtime_binding(cwd, controller.clone())
        } else {
            runtime_binding(cwd, controller.clone())
        }
        .map_err(|error| error.to_string())?
        .target_binding();
        let workspace = cwd.to_path_buf();
        let process_lifecycle = self.process_lifecycle;
        let resumed_profile_target =
            profile_fork && session.model.as_deref() == Some("claude-opus-test");
        let rebound = Arc::new(AtomicBool::new(false));
        let runtime_generation = Arc::new(AtomicU64::new(1));
        let runtime = Arc::new(move |role: RuntimeRole| {
            if profile_fork {
                let account = if resumed_profile_target || role == RuntimeRole::Rebind {
                    b"tui-target-claude".as_slice()
                } else {
                    b"tui-source-grok".as_slice()
                };
                runtime_binding_for_account(&workspace, controller.clone(), account)
            } else if long_work {
                if role == RuntimeRole::Rebind {
                    rebound.store(true, Ordering::Release);
                }
                let account = if rebound.load(Ordering::Acquire) {
                    b"long-work-target".as_slice()
                } else {
                    b"long-work-source".as_slice()
                };
                runtime_binding_for_account_with_identity_namespace(
                    &workspace,
                    controller.clone(),
                    account,
                    u128::from(runtime_generation.fetch_add(1, Ordering::AcqRel)),
                )
            } else if process_lifecycle {
                exited_child_process_runtime_binding(&workspace, controller.clone())
            } else {
                runtime_binding(&workspace, controller.clone())
            }
            .map_err(|error| error.to_string())
        });
        let injection = Arc::new(ScriptedRuntimeFactory {
            runtime,
            rebind_target: target,
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: Mutex::new(Vec::new()),
        });
        let model_id = if profile_fork {
            session
                .model
                .clone()
                .unwrap_or_else(|| "offline-loopback-v1".to_owned())
        } else {
            "offline-loopback-v1".to_owned()
        };
        let attached = crate::product_host::spawn_host_worker(HostConfig {
            surface: Some(crate::shell::settings::ProductSurface::boxed(paths)),
            auto_continue: true,
            topology_admission: None,
            state_roots: state_roots.clone(),
            product_home: paths.home().to_path_buf(),
            home: paths.session_home(),
            store: Arc::new(crate::store::session_records::SqliteSessionStore::new(
                paths.clone(),
            )),
            session,
            target: plakous_hosted::runtime::test_kit::registered_authority(NativeProvider::Codex),
            model_id,
            effort: Some(effort),
            effort_explicit,
            compaction_enabled: false,
            max_model_rounds: crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_enabled: crate::product_config::SessionPolicy::load(paths)
                .unwrap()
                .topology_enabled,
            root_template: None,
            attached_one_shot: false,
            interactive: true,
            cwd: cwd.to_path_buf(),
            ready_catalog: if profile_fork {
                tui_profile_ready_catalog()
            } else if long_work {
                long_work_ready_catalog()
            } else {
                ready_catalog.clone()
            },
            resume_publication,
            runtime_factory: injection,
            ready_path: crate::product_host::ReadyPathTiming::default(),
            seed_history: None,
            source_releases: crate::product_host::SourceReleases::default(),
        });
        if let Err(error) = &attached {
            let _ = std::fs::write(paths.home().join("pty-attach-error.txt"), error.message());
        }
        attached.map_err(|failure| failure.message().to_owned())
    }
}

#[test]
#[ignore = "on-demand real-terminal qualification"]
fn terminal_e2e_child() -> Result<(), Box<dyn Error>> {
    if std::env::var_os("PLAK_PTY_CHILD").is_none() {
        return Ok(());
    }
    let home = PathBuf::from(std::env::var_os("PLAK_HOME").ok_or("PLAK_HOME missing")?);
    let paths = PlakPaths::explicit(home);
    let mut config = crate::product_config::ProductConfig::default_product();
    if std::env::var_os("PLAK_PTY_PROFILE_FORK").is_some() {
        config.bind_root_tuple("grok", "grok-4.6", Some("medium"));
    } else {
        config.bind_root_tuple("codex", "gpt-5.6-luna", Some("medium"));
    }
    config.compaction_enabled = false;
    config.save(&paths)?;
    let resume = std::env::var("PLAK_PTY_RESUME").ok();
    let resume_picker = std::env::var_os("PLAK_PTY_RESUME_PICKER").is_some();
    let attach_refusals = std::env::var("PLAK_PTY_ATTACH_REFUSALS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let evidence =
        PathBuf::from(std::env::var_os("PLAK_PTY_EVIDENCE").ok_or("PLAK_PTY_EVIDENCE missing")?);
    let services = crate::shell::services::Services::from_env(None);
    let outcome = launch_tui_with_attacher(
        &services,
        resume,
        resume_picker,
        None,
        &OfflineAttacher {
            evidence,
            process_lifecycle: std::env::var_os("PLAK_PTY_PROCESS").is_some(),
            profile_fork: std::env::var_os("PLAK_PTY_PROFILE_FORK").is_some(),
            long_work_release: std::env::var_os("PLAK_PTY_LONG_WORK_RELEASE").map(PathBuf::from),
            stream_gate_release: std::env::var_os("PLAK_PTY_STREAM_GATE_RELEASE")
                .map(PathBuf::from),
            effect_gate_release: std::env::var_os("PLAK_PTY_EFFECT_GATE_RELEASE")
                .map(PathBuf::from),
            remaining_attach_refusals: AtomicUsize::new(attach_refusals),
        },
    )
    .map_err(|failure| failure.message().to_owned())?;
    if outcome != ProcessOutcome::Success {
        return Err("PTY child did not exit successfully".into());
    }
    Ok(())
}

struct PtyChild {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    terminal: Arc<Mutex<TerminalCapture>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    rows: u16,
    cols: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrameSnapshot {
    contents: String,
    cursor: (u16, u16),
    cursor_hidden: bool,
    raw_len: usize,
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

    fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(rows, cols);
    }

    fn snapshot(&self) -> FrameSnapshot {
        FrameSnapshot {
            contents: self.parser.screen().contents(),
            cursor: self.parser.screen().cursor_position(),
            cursor_hidden: self.parser.screen().hide_cursor(),
            raw_len: self.raw.len(),
        }
    }
}

/// Everything a real-terminal child launch varies, so the spawn helper keeps a
/// short signature as options accumulate.
#[derive(Default)]
struct PtyOptions<'a> {
    resume: Option<&'a str>,
    process_lifecycle: bool,
    profile_fork: bool,
    long_work_release: Option<&'a Path>,
    gate: Option<TurnGate<'a>>,
    /// Launch as `plak resume` / `plak --resume`: the picker opens over the
    /// first frame.
    resume_picker: bool,
    /// Attach attempts the injected attacher refuses before it succeeds.
    attach_refusals: usize,
}

impl PtyChild {
    fn spawn(
        home: &Path,
        cwd: &Path,
        evidence: &Path,
        resume: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_options(
            home,
            cwd,
            evidence,
            PtyOptions {
                resume,
                ..PtyOptions::default()
            },
        )
    }

    fn spawn_process(home: &Path, cwd: &Path, evidence: &Path) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_options(
            home,
            cwd,
            evidence,
            PtyOptions {
                process_lifecycle: true,
                ..PtyOptions::default()
            },
        )
    }

    fn spawn_profile_fork(
        home: &Path,
        cwd: &Path,
        evidence: &Path,
        resume: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_options(
            home,
            cwd,
            evidence,
            PtyOptions {
                resume,
                profile_fork: true,
                ..PtyOptions::default()
            },
        )
    }

    /// Launch the resume picker over a shell that refuses `attach_refusals`
    /// attach attempts first.
    fn spawn_resume_picker(
        home: &Path,
        cwd: &Path,
        evidence: &Path,
        attach_refusals: usize,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_options(
            home,
            cwd,
            evidence,
            PtyOptions {
                resume_picker: true,
                attach_refusals,
                ..PtyOptions::default()
            },
        )
    }

    fn spawn_long_work(
        home: &Path,
        cwd: &Path,
        evidence: &Path,
        release: &Path,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_options(
            home,
            cwd,
            evidence,
            PtyOptions {
                long_work_release: Some(release),
                ..PtyOptions::default()
            },
        )
    }

    /// Spawn with the first provider response stream parked until `release`
    /// appears, so the TUI sits on a genuinely cancellable streaming turn.
    fn spawn_gated(
        home: &Path,
        cwd: &Path,
        evidence: &Path,
        gate: TurnGate<'_>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_options(
            home,
            cwd,
            evidence,
            PtyOptions {
                gate: Some(gate),
                ..PtyOptions::default()
            },
        )
    }

    fn spawn_with_options(
        home: &Path,
        cwd: &Path,
        evidence: &Path,
        options: PtyOptions<'_>,
    ) -> Result<Self, Box<dyn Error>> {
        let PtyOptions {
            resume,
            process_lifecycle,
            profile_fork,
            long_work_release,
            gate,
            resume_picker,
            attach_refusals,
        } = options;
        let pair = native_pty_system().openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(std::env::current_exe()?);
        command.args([
            "--ignored",
            "--exact",
            CHILD_TEST,
            "--nocapture",
            "--test-threads=1",
        ]);
        command.env_clear();
        command.env("HOME", home);
        command.env("PLAK_HOME", home);
        command.env("XDG_CONFIG_HOME", home.join("xdg-config"));
        command.env("XDG_DATA_HOME", home.join("xdg-data"));
        command.env("XDG_STATE_HOME", home.join("xdg-state"));
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        command.env("LANG", "C.UTF-8");
        command.env("PLAK_PTY_CHILD", "1");
        command.env("PLAK_PTY_EVIDENCE", evidence);
        if process_lifecycle {
            command.env("PLAK_PTY_PROCESS", "1");
        }
        if profile_fork {
            command.env("PLAK_PTY_PROFILE_FORK", "1");
        }
        if let Some(release) = long_work_release {
            command.env("PLAK_PTY_LONG_WORK_RELEASE", release);
        }
        match gate {
            Some(TurnGate::Stream(release)) => {
                command.env("PLAK_PTY_STREAM_GATE_RELEASE", release);
            }
            Some(TurnGate::Effect(release)) => {
                command.env("PLAK_PTY_EFFECT_GATE_RELEASE", release);
            }
            None => {}
        }
        if let Some(id) = resume {
            command.env("PLAK_PTY_RESUME", id);
        }
        if resume_picker {
            command.env("PLAK_PTY_RESUME_PICKER", "1");
        }
        if attach_refusals > 0 {
            command.env("PLAK_PTY_ATTACH_REFUSALS", attach_refusals.to_string());
        }
        command.cwd(cwd);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let terminal = Arc::new(Mutex::new(TerminalCapture::new(ROWS, COLS)));
        let sink = Arc::clone(&terminal);
        let reader_thread = std::thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                sink.lock().unwrap().process(&buffer[..count]);
            }
        });
        Ok(Self {
            master: pair.master,
            writer,
            child,
            terminal,
            reader_thread: Some(reader_thread),
            rows: ROWS,
            cols: COLS,
        })
    }

    fn frame(&self) -> FrameSnapshot {
        self.terminal.lock().unwrap().snapshot()
    }

    fn screen(&self) -> String {
        self.frame().contents
    }

    fn output_len(&self) -> usize {
        self.frame().raw_len
    }

    fn footer_row(&self) -> usize {
        // The composer footer is anchored to the PTY's final visible row.
        usize::from(self.rows.saturating_sub(1))
    }

    fn wait_output_after(&mut self, baseline: usize) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + WAIT;
        loop {
            if self.output_len() > baseline {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                return Err(format!("PTY child exited before redraw: {status:?}").into());
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for PTY redraw".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_screen_row_contains(&mut self, row: usize, needle: &str) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + WAIT;
        loop {
            let first = self.frame();
            if first
                .contents
                .lines()
                .nth(row)
                .is_some_and(|line| line.contains(needle))
            {
                std::thread::sleep(Duration::from_millis(30));
                if first == self.frame() {
                    return Ok(());
                }
            }
            if let Some(status) = self.child.try_wait()? {
                return Err(format!(
                    "PTY child exited before stable row {row} contained `{needle}`: {status:?}\n{}",
                    first.contents
                )
                .into());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for stable row {row} to contain `{needle}`\n{}",
                    first.contents
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_screen(&mut self, needle: &str) -> Result<String, Box<dyn Error>> {
        self.wait_stable_where(&format!("screen containing `{needle}`"), |frame| {
            frame.contains(needle)
        })
    }

    fn wait_stable_where(
        &mut self,
        description: &str,
        predicate: impl Fn(&str) -> bool,
    ) -> Result<String, Box<dyn Error>> {
        let deadline = Instant::now() + WAIT;
        loop {
            let first = self.frame();
            if predicate(&first.contents) {
                std::thread::sleep(Duration::from_millis(30));
                let second = self.frame();
                if first == second && predicate(&second.contents) {
                    return Ok(second.contents);
                }
            }
            if let Some(status) = self.child.try_wait()? {
                return Err(format!(
                    "PTY child exited before stable {description}: {status:?}\n{}",
                    first.contents
                )
                .into());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for stable {description}\n{}",
                    first.contents
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Box<dyn Error>> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.terminal.lock().unwrap().resize(rows, cols);
        self.rows = rows;
        self.cols = cols;
        Ok(())
    }

    /// Type `text` and wait until the composer shows it on a stable frame.
    ///
    /// The draft is never submitted, so the tokens can only come from the
    /// composer: the transcript is untouched by an unsent draft.
    fn type_draft(&mut self, text: &str) -> Result<(), Box<dyn Error>> {
        self.send(text.as_bytes())?;
        self.wait_stable_where(&format!("composer draft `{text}`"), |screen| {
            screen.contains(text)
        })?;
        Ok(())
    }

    fn wait_draft_absent(&mut self, text: &str) -> Result<(), Box<dyn Error>> {
        self.wait_stable_where(&format!("cleared draft `{text}`"), |screen| {
            !screen.contains(text)
        })?;
        Ok(())
    }

    /// Send the §6.1 idle Esc-Esc pair until the armed draft clears.
    ///
    /// The second Esc must land inside the 800 ms arming window, which a loaded
    /// CI box can miss; re-arming is idempotent, so the pair is retried rather
    /// than made flaky by one scheduling stall.
    fn esc_esc_clear_draft(&mut self, draft: &str) -> Result<(), Box<dyn Error>> {
        for _ in 0..5 {
            self.send(b"\x1b")?;
            std::thread::sleep(Duration::from_millis(40));
            self.send(b"\x1b")?;
            let deadline = Instant::now() + Duration::from_millis(600);
            while Instant::now() < deadline {
                if !self.screen().contains(draft) {
                    return Ok(());
                }
                if let Some(status) = self.child.try_wait()? {
                    return Err(format!("PTY child exited during Esc-Esc: {status:?}").into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Err(format!(
            "Esc-Esc never cleared the draft `{draft}`\n{}",
            self.screen()
        )
        .into())
    }

    /// The last tool *summary* line, identified by its header glyph so the
    /// expanded detail rail — which repeats the subject — is never matched.
    fn tool_row(&self, subject: &str) -> Result<String, Box<dyn Error>> {
        self.screen()
            .lines()
            .rev()
            .find(|line| tool_summary_row(line, subject))
            .map(str::to_owned)
            .ok_or_else(|| format!("no tool row for `{subject}`\n{}", self.screen()).into())
    }

    fn quit_and_wait(mut self) -> Result<Vec<u8>, Box<dyn Error>> {
        self.send(b"/quit\r")?;
        self.wait_exit()
    }

    /// Wait until the raw byte stream the TUI wrote contains `needle`.
    fn wait_raw_contains(&mut self, needle: &[u8]) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + WAIT;
        loop {
            if self
                .terminal
                .lock()
                .unwrap()
                .raw
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("timed out waiting for raw bytes {needle:?}").into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Await a clean child exit and return every byte the TUI wrote.
    fn wait_exit(self) -> Result<Vec<u8>, Box<dyn Error>> {
        let (status, raw) = self.wait_exit_status()?;
        if !status.success() {
            return Err(format!("PTY child failed: {status:?}").into());
        }
        Ok(raw)
    }

    /// Deliver `signal` to the child process, as a supervisor or a closing
    /// terminal would.
    fn signal(&self, signal: rustix::process::Signal) -> Result<(), Box<dyn Error>> {
        let pid = self
            .child
            .process_id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
            .ok_or("PTY child has no pid")?;
        rustix::process::kill_process(pid, signal)?;
        Ok(())
    }

    /// Await the child's exit, whatever its status, with every byte it wrote.
    fn wait_exit_status(mut self) -> Result<(portable_pty::ExitStatus, Vec<u8>), Box<dyn Error>> {
        let deadline = Instant::now() + WAIT;
        // A quit that lands while a turn still settles asks first (spec 02
        // §5.3): confirm it once, as the operator would.
        let mut confirmed = false;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                return Err("PTY child required forced cleanup".into());
            }
            if !confirmed && self.screen().contains("Quit plak?") {
                confirmed = true;
                // Focus starts on Quit, so Enter confirms. Best effort: the
                // test may have confirmed already and the TUI may be gone;
                // macOS reports a write to a closed pty as EIO.
                let _ = self.send(b"\r");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if let Some(reader_thread) = self.reader_thread.take() {
            reader_thread
                .join()
                .map_err(|_| "PTY reader thread panicked")?;
        }
        let raw = self.terminal.lock().unwrap().raw.clone();
        Ok((status, raw))
    }
}

#[test]
fn injected_real_pty_model_response_and_slash_clear() -> Result<(), Box<dyn Error>> {
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&cwd)?;
    let evidence = root.path().join("physical.jsonl");
    let paths = PlakPaths::explicit(home.clone());

    let startup = Instant::now();
    let mut child = PtyChild::spawn(&home, &cwd, &evidence, None)?;
    if let Err(error) = child.wait_screen("/10M") {
        let attach = std::fs::read_to_string(home.join("pty-attach-error.txt"))
            .unwrap_or_else(|_| "no attach diagnostic was recorded".to_owned());
        let events = std::fs::read_dir(PlakPaths::explicit(home.clone()).sessions_dir())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "jsonl")
            })
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("{error}; attach: {attach}; events: {events}").into());
    }
    assert!(
        startup.elapsed() <= latency_budget(Duration::from_secs(6)),
        "offline TUI startup exceeded the interactive budget: {:?}",
        startup.elapsed()
    );
    let settings = Instant::now();
    child.send(b"/settings\r")?;
    child.wait_screen("Menu")?;
    assert!(
        settings.elapsed() <= latency_budget(Duration::from_millis(500)),
        "settings first frame exceeded the interactive budget: {:?}",
        settings.elapsed()
    );
    child.send(b"\x1b")?;
    child.wait_stable_where("settings closed", |screen| !screen.contains("Menu"))?;
    child.send(b"/agents\r")?;
    child.wait_screen("choose when it applies")?;
    child.send(b"\x1b")?;
    child.wait_stable_where("Menu closed", |screen| !screen.contains("Menu"))?;

    let prompt = "clear this completed rendered turn";
    child.send(format!("{prompt}\r").as_bytes())?;
    child.wait_screen("loopback turn committed")?;
    let clear = Instant::now();
    child.send(b"/clear\r")?;
    let cleared = child.wait_stable_where("cleared shell without the prior turn", |screen| {
        screen.contains("%/")
            && screen.contains("/ commands")
            && !screen.contains(prompt)
            && !screen.contains("loopback turn committed")
    })?;
    assert!(
        clear.elapsed() <= latency_budget(Duration::from_secs(2)),
        "offline clear exceeded the interactive budget: {:?}",
        clear.elapsed()
    );
    assert!(!cleared.contains("recovery required"), "{cleared}");
    let cleared_session = wait_product_event(&paths, &cwd, "session.cleared")?;
    assert_ne!(cleared_session, "");

    child.send(b"turn after clear\r")?;
    child.wait_screen("loopback turn committed")?;
    child.send(b"/fork\r")?;
    child.wait_screen("forked session")?;
    child.send(b"turn after fork\r")?;
    let continued = child.wait_screen("loopback turn committed")?;
    assert!(!continued.contains("recovery required"), "{continued}");
    // Quit is process-exit, not joined shutdown. Wait until the host is idle
    // so the last turn's finalization is the head, not a RecoveryRequired
    // checkpoint a kill would freeze.
    child.wait_stable_where("idle after the forked turn", |screen| {
        screen.contains("loopback turn committed")
            && screen.contains("/ commands")
            && !screen.contains("thinking")
            && !screen.contains("responding")
    })?;

    let raw = child.quit_and_wait()?;
    assert!(raw.windows(7).any(|window| window == b"[?1000l"));
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));
    let sessions = product_sessions::list_sessions_for_cwd(&paths, &cwd)?;
    let forked = sessions
        .iter()
        .filter_map(|projection| product_sessions::load_session(&paths, &projection.id).ok())
        .find(|session| {
            session.transcript.iter().any(|line| {
                line.text()
                    .is_some_and(|text| text.contains("turn after fork"))
            })
        })
        .ok_or("post-fork prompt was not committed to a product session")?;
    assert!(
        forked.transcript.iter().any(|line| line
            .text()
            .is_some_and(|text| text.contains("loopback turn committed"))),
        "post-fork session omitted its authoritative assistant projection: {:?}",
        forked.transcript
    );
    let locator = forked
        .plakous_locator
        .ok_or("post-fork product session omitted its runtime locator")?;
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
    let runtime = runtime_binding(&cwd, controller)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let runtime_sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let executor = ApplicationExecutor::new()?;
    let reopened = executor.block_on(runtime_sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("clear-fork-pty:reopen-terminal")?,
        locator,
        runtime,
    )))??;
    let OpenSessionOutcome::Opened { session, .. } = reopened else {
        return Err(
            format!("post-fork runtime was not terminally reopenable: {reopened:?}").into(),
        );
    };
    let _ = executor.block_on(session.release())??;
    Ok(())
}

#[test]
fn injected_real_pty_long_work_transitions_remain_ordered_and_reopen_quiescent()
-> Result<(), Box<dyn Error>> {
    const FIRST: &str = "long work first root";
    const FOLLOW_UP: &str = "long work queued follow-up";
    const AFTER_FORK: &str = "prompt after exact fork";
    const AFTER_CLEAR: &str = "prompt after clear";

    let root = tempfile::tempdir()?;
    let home = root.path().join("home");
    let cwd = root.path().join("workspace");
    let evidence = root.path().join("long-work-physical.jsonl");
    let release = root.path().join("release-first-provider");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&cwd)?;
    let paths = PlakPaths::explicit(home.clone());

    let mut child = PtyChild::spawn_long_work(&home, &cwd, &evidence, &release)?;
    if let Err(error) = child.wait_screen("/10M") {
        let attach = std::fs::read_to_string(home.join("pty-attach-error.txt"))
            .unwrap_or_else(|_| "no attach diagnostic was recorded".to_owned());
        let events = std::fs::read_dir(PlakPaths::explicit(home.clone()).sessions_dir())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "jsonl")
            })
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("{error}; attach: {attach}; events: {events}").into());
    }
    child.send(format!("{FIRST}\r").as_bytes())?;
    wait_physical_evidence(&evidence, EvidenceKind::ProviderDispatched, 1)?;
    child.wait_screen("waiting for model")?;

    let follow_up_ack = Instant::now();
    child.send(format!("{FOLLOW_UP}\r").as_bytes())?;
    child.wait_screen(FOLLOW_UP)?;
    assert!(
        follow_up_ack.elapsed() <= latency_budget(Duration::from_secs(3)),
        "busy follow-up acknowledgement exceeded the debug-test interactive budget: {:?}",
        follow_up_ack.elapsed()
    );

    let settings = Instant::now();
    child.send(b"/settings\r")?;
    child.wait_screen("Menu")?;
    assert!(
        settings.elapsed() <= latency_budget(Duration::from_secs(3)),
        "busy settings frame exceeded the debug-test interactive budget: {:?}",
        settings.elapsed()
    );
    assert_eq!(
        read_evidence(&evidence)?
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
            .count(),
        1,
        "settings and follow-up must not release or duplicate the gated provider"
    );
    assert_eq!(
        read_evidence(&evidence)?
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderOutcome)
            .count(),
        0,
        "the first provider must remain physically gated while settings is responsive"
    );
    child.send(b"\x1b")?;
    child.wait_stable_where("busy settings closed", |screen| {
        !screen.contains("Menu") && screen.contains(FOLLOW_UP)
    })?;

    let gate_release = Instant::now();
    std::fs::write(&release, b"release")?;
    let queued_completed = child.wait_stable_where("ordered queued completion", |screen| {
        screen.contains(FIRST)
            && screen.contains(FOLLOW_UP)
            && screen.matches("loopback turn committed").count() >= 2
            && !screen.contains("thinking")
            && !screen.contains("waiting for model")
    })?;
    let queued = wait_prompt_committed(&paths, &cwd, FOLLOW_UP)?;
    assert!(
        gate_release.elapsed() <= latency_budget(Duration::from_secs(30)),
        "gate release through both ordered roots exceeded the debug-test containment budget: {:?}",
        gate_release.elapsed()
    );
    assert!(
        !queued_completed.contains("recovery required"),
        "{queued_completed}"
    );
    assert!(
        !queued_completed.contains("thinking") && !queued_completed.contains("waiting for model"),
        "terminal completion did not restore the idle presentation: {queued_completed}"
    );
    assert_eq!(queued.prompt_history, [FIRST, FOLLOW_UP]);

    let fork = Instant::now();
    child.send(b"/fork\r")?;
    child.wait_screen("forked session")?;
    assert!(
        fork.elapsed() <= latency_budget(Duration::from_secs(3)),
        "fork publication exceeded the interactive budget: {:?}",
        fork.elapsed()
    );
    child.send(format!("{AFTER_FORK}\r").as_bytes())?;
    child.wait_stable_where("prompt completed after fork", |screen| {
        screen.contains(AFTER_FORK) && screen.contains("loopback turn committed")
    })?;
    let forked = wait_prompt_committed(&paths, &cwd, AFTER_FORK).map_err(|error| {
        format!(
            "{error}\nterminal:\n{}\nphysical evidence: {:?}",
            child.screen(),
            read_evidence(&evidence).unwrap_or_default()
        )
    })?;
    assert_eq!(forked.prompt_history, [FIRST, FOLLOW_UP, AFTER_FORK]);
    {
        let mut store = crate::store::ProductStore::open(&paths)?;
        let source_id = forked
            .source_product_session_id
            .as_deref()
            .ok_or("fork omitted its source product identity")?;

        let intents = store.load_intents(source_id)?;
        let request = intents
            .workflow_requests
            .iter()
            .find(|request| request.kind == product_sessions::SessionWorkflowKind::Fork)
            .ok_or("plain fork omitted its immutable workflow request")?;
        assert_eq!(request.successor_product_id(), forked.id);
        let views = store
            .load_views(source_id)?
            .ok_or("fork source omitted its views")?;
        assert!(
            views.state.workflows[&request.operation_id]
                .settled
                .is_some()
        );
    }

    let clear = Instant::now();
    child.send(b"/clear\r")?;
    let clear_sent = clear.elapsed();
    let clear_first_visible = std::cell::Cell::new(None);
    child.wait_stable_where("fresh shell after clear", |screen| {
        let visible = screen.contains("/ commands") && !screen.contains(AFTER_FORK);
        if visible && clear_first_visible.get().is_none() {
            clear_first_visible.set(Some(clear.elapsed()));
        }
        visible
    })?;
    let clear_stable = clear.elapsed();
    eprintln!(
        "clear stages: input_written={clear_sent:?}, first_visible={:?}, stable={clear_stable:?}",
        clear_first_visible.get()
    );
    assert!(
        clear_stable <= latency_budget(Duration::from_secs(3)),
        "clear publication exceeded the interactive budget: input_written={clear_sent:?}, first_visible={:?}, stable={clear_stable:?}",
        clear_first_visible.get()
    );
    child.send(format!("{AFTER_CLEAR}\r").as_bytes())?;
    child.wait_stable_where("prompt completed after clear", |screen| {
        screen.contains(AFTER_CLEAR) && screen.contains("loopback turn committed")
    })?;
    let cleared = wait_prompt_committed(&paths, &cwd, AFTER_CLEAR)?;
    assert_eq!(cleared.prompt_history, [AFTER_CLEAR]);
    {
        let mut store = crate::store::ProductStore::open(&paths)?;
        let intents = store.load_intents(&forked.id)?;
        let request = intents
            .workflow_requests
            .iter()
            .find(|request| request.kind == product_sessions::SessionWorkflowKind::Clear)
            .ok_or("clear omitted its immutable workflow request")?;
        assert_eq!(request.successor_product_id(), cleared.id);
        assert_eq!(
            cleared.create_operation_id,
            format!("{}:create", request.operation_id)
        );

        let views = store
            .load_views(&forked.id)?
            .ok_or("clear source omitted its views")?;
        assert!(
            views.state.workflows[&request.operation_id]
                .settled
                .is_some()
        );
    }

    let raw = child.quit_and_wait()?;
    assert!(raw.windows(7).any(|window| window == b"[?1000l"));
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));

    let records = product_sessions::list_sessions_for_cwd(&paths, &cwd)?
        .into_iter()
        .map(|row| product_sessions::load_session(&paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(records.len(), 3, "source, fork, and clear only");
    let physical_before_reopen = read_evidence(&evidence)?;
    assert_eq!(
        physical_before_reopen
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
            .count(),
        7,
        "four roots plus one first-turn tool continuation for each fresh runtime binding"
    );
    assert_eq!(
        physical_before_reopen
            .iter()
            .filter(|row| row.kind == EvidenceKind::EffectDispatched)
            .count(),
        3
    );

    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let executor = ApplicationExecutor::new()?;
    for (index, record) in records.iter().enumerate() {
        let locator = record.plakous_locator.ok_or_else(|| {
            format!(
                "surviving product session {} omitted its locator",
                record.id
            )
        })?;
        let target_account = if record.model.as_deref() == Some("offline-loopback-v2") {
            b"long-work-target".as_slice()
        } else {
            b"long-work-source".as_slice()
        };
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
        let opened = executor.block_on(
            sessions.open(OpenSessionRequest::new(
                SessionOperationId::new(format!("long-work-pty:reopen-{index}"))?,
                locator,
                runtime_binding_for_account(&cwd, controller, target_account)
                    .map_err(|error| std::io::Error::other(error.to_string()))?,
            )),
        )??;
        let OpenSessionOutcome::Opened { session, .. } = opened else {
            return Err(format!(
                "surviving session {} was not quiescent through the public facade: {opened:?}",
                record.id
            )
            .into());
        };
        let _ = executor.block_on(session.release())??;
    }
    assert_eq!(read_evidence(&evidence)?, physical_before_reopen);
    Ok(())
}

#[cfg(unix)]
#[test]
fn injected_real_pty_exited_child_over_8192_continues_without_recovery_required()
-> Result<(), Box<dyn Error>> {
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&cwd)?;
    let evidence = root.path().join("process-physical.jsonl");
    let paths = PlakPaths::explicit(home.clone());

    let mut child = PtyChild::spawn_process(&home, &cwd, &evidence)?;
    child.wait_screen("/ commands")?;
    child.send(b"reproduce the exited child continuation\r")?;
    let terminal = child
        .wait_screen("large exited child continued and joined")
        .map_err(|error| {
            let events = product_sessions::list_sessions_for_cwd(&paths, &cwd)
                .unwrap_or_default()
                .into_iter()
                .map(|projection| {
                    std::fs::read_to_string(paths.session_events_file(&projection.id))
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let attach = std::fs::read_to_string(paths.home().join("pty-attach-error.txt"))
                .unwrap_or_default();
            format!("{error}\nattach error: {attach}\nproduct events:\n{events}")
        })?;
    assert!(terminal.contains("large exited child continued and joined"));
    assert!(!terminal.contains("recovery required"), "{terminal}");
    let raw = child.quit_and_wait()?;
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));

    let projections = product_sessions::list_sessions_for_cwd(&paths, &cwd)?;
    let [projection] = projections.as_slice() else {
        return Err("process PTY journey did not leave exactly one product session".into());
    };
    let session = product_sessions::load_session(&paths, &projection.id)?;
    let activities = session
        .transcript
        .iter()
        .filter_map(|row| row.as_tool().cloned())
        .collect::<Vec<_>>();
    assert_eq!(activities.len(), 2, "{activities:#?}");
    assert_eq!(activities[0].name, "process_session_start");
    assert_eq!(activities[0].state, "succeeded");
    assert_eq!(activities[1].name, "process_session_poll");
    assert_eq!(activities[1].state, "succeeded");
    assert!(session.transcript.iter().any(|line| {
        line.text()
            .is_some_and(|text| text.contains("large exited child continued and joined"))
    }));

    // Product transcript/events are observations only. Reopen the exact runtime
    // locator to prove this is a quiescent terminal checkpoint, not a rendered
    // success over a RecoveryRequired branch.
    let locator = session
        .plakous_locator
        .ok_or("process PTY journey omitted its runtime locator")?;
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
    let runtime = exited_child_process_runtime_binding(&cwd, controller)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let executor = ApplicationExecutor::new()?;
    let opened = executor.block_on(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("process-pty:reopen-terminal")?,
        locator,
        runtime,
    )))??;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err(
            format!("process PTY locator was not terminally reopenable: {opened:?}").into(),
        );
    };
    let _ = executor.block_on(session.release())??;
    Ok(())
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(reader_thread) = self.reader_thread.take() {
            let _ = reader_thread.join();
        }
    }
}

fn wait_product_event(paths: &PlakPaths, cwd: &Path, kind: &str) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + WAIT;
    loop {
        for projection in product_sessions::list_sessions_for_cwd(paths, cwd)? {
            let events = std::fs::read_to_string(paths.session_events_file(&projection.id))
                .unwrap_or_default();
            if events.lines().any(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|event| {
                        event
                            .get("kind")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .is_some_and(|observed| observed == kind)
            }) {
                return Ok(projection.id);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for product event `{kind}`").into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_prompt_committed(
    paths: &PlakPaths,
    cwd: &Path,
    prompt: &str,
) -> Result<SessionRecord, Box<dyn Error>> {
    let deadline = Instant::now() + WAIT;
    loop {
        for projection in product_sessions::list_sessions_for_cwd(paths, cwd)? {
            let session = product_sessions::load_session(paths, &projection.id)?;
            if session
                .prompt_history
                .last()
                .is_some_and(|value| value == prompt)
                && session
                    .transcript
                    .iter()
                    .filter(|line| {
                        line.text()
                            .is_some_and(|text| text.contains("loopback turn committed"))
                    })
                    .count()
                    >= session.prompt_history.len()
            {
                return Ok(session);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for committed prompt `{prompt}`").into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_physical_evidence(
    evidence: &Path,
    kind: EvidenceKind,
    count: usize,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + WAIT;
    loop {
        if read_evidence(evidence)?
            .iter()
            .filter(|row| row.kind == kind)
            .count()
            >= count
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                format!("timed out waiting for physical {kind:?} evidence #{count}").into(),
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_completed_loopback_activity(session: &SessionRecord) -> Result<String, Box<dyn Error>> {
    let activities = session
        .transcript
        .iter()
        .filter_map(|row| row.as_tool().cloned())
        .collect::<Vec<_>>();
    let [activity] = activities.as_slice() else {
        return Err(format!(
            "expected one persisted tool activity, found {} in {:?}",
            activities.len(),
            session.transcript
        )
        .into());
    };
    assert_eq!(activity.name, "loopback_effect");
    assert_eq!(activity.state, "succeeded");
    assert!(
        activity.started_at_ms.is_some_and(|started| activity
            .finished_at_ms
            .is_some_and(|finished| finished >= started)),
        "completed activity must retain an ordered time range: {activity:?}"
    );
    let detail = activity
        .detail
        .as_deref()
        .ok_or("completed activity omitted its expandable detail")?;
    // The expand body is readable text projected by
    // `activity::tool_detail_text`, never `Arguments:` / `Result:` JSON
    // scaffolding.
    assert!(
        !detail.contains("Arguments:") && !detail.contains("Result:"),
        "{detail}"
    );
    assert!(detail.contains("loopback effect committed"), "{detail}");
    Ok(activity.call_id.clone())
}

/// `Esc` on the launch resume picker must not strand the product.
///
/// `plak resume` and `plak --resume` park the launch on a shell host with
/// nothing attached while the operator chooses. Declining used to leave the
/// header on `not ready · pick a session to resume` forever and swallow the
/// next prompt as `queued until agent is ready`; the only escape was an
/// `/attach` nothing on screen mentioned. Declining now attaches the
/// workspace's default model, exactly as a bare `plak` does, and says so.
#[test]
fn real_pty_declining_the_launch_resume_picker_attaches_the_default() -> Result<(), Box<dyn Error>>
{
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&cwd)?;
    let evidence = root.path().join("physical.jsonl");

    // One refusal reproduces the launch state the picker opens over: a shell
    // host with no agent attached. The reattach the decline asks for then
    // succeeds, which is the workspace that does have a default.
    let mut child = PtyChild::spawn_resume_picker(&home, &cwd, &evidence, 1)?;
    child.wait_screen("Resume session")?;
    let picking = child.screen();
    assert!(
        picking.contains("awaiting"),
        "the picker opens over a shell with nothing attached:\n{picking}"
    );

    child.send(b"\x1b")?;
    child.wait_screen("/10M")?;
    let attached = child.wait_screen("attached this workspace's default model")?;
    assert!(
        !attached.contains("awaiting context"),
        "declining must leave the not-ready shell behind:\n{attached}"
    );
    assert!(
        attached.contains("attached this workspace's default model"),
        "declining must say what it did instead of hiding `/attach`:\n{attached}"
    );

    // The defect that made this Medium: the first prompt after the decline was
    // absorbed by the unattached session and never dispatched.
    child.send("hello after declining\r".as_bytes())?;
    let committed = child.wait_screen("loopback turn committed")?;
    assert!(
        committed.contains("hello after declining"),
        "the prompt typed after declining must dispatch, not queue:\n{committed}"
    );

    let raw = child.quit_and_wait()?;
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));
    Ok(())
}

/// No attachable default means a clean quit.
///
/// Opening another not-ready shell would strand the operator exactly as before,
/// so the launcher restores the terminal and says why it stopped.
#[test]
fn real_pty_declining_the_picker_without_a_default_quits_cleanly() -> Result<(), Box<dyn Error>> {
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&cwd)?;
    let evidence = root.path().join("physical.jsonl");

    // A refusal count no round can exhaust: this workspace has no attachable
    // default at all.
    let mut child = PtyChild::spawn_resume_picker(&home, &cwd, &evidence, 64)?;
    child.wait_screen("Resume session")?;
    child.send(b"\x1b")?;

    let raw = child.wait_exit()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        text.contains("no session resumed, and this workspace has no attachable default"),
        "the quit must say why it stopped:\n{text}"
    );
    assert!(
        raw.windows(7).any(|window| window == b"[?1049l"),
        "the alternate screen is left on the way out"
    );
    Ok(())
}

#[test]
fn real_pty_fresh_prompt_resume_and_restore() -> Result<(), Box<dyn Error>> {
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&cwd)?;
    let evidence = root.path().join("physical.jsonl");
    let paths = PlakPaths::explicit(home.clone());

    let mut fresh = PtyChild::spawn(&home, &cwd, &evidence, None)?;
    fresh.wait_screen("/10M")?;
    let before_resize = fresh.output_len();
    fresh.resize(40, 120)?;
    fresh.wait_output_after(before_resize)?;
    fresh.wait_screen_row_contains(fresh.footer_row(), "/ commands")?;
    // SGR mouse-wheel input and keyboard submission share the real terminal
    // event loop; the wheel event must be harmless and leave input usable.
    fresh.send(b"\x1b[<65;10;10M")?;
    fresh.send("hello from a real PTY ✓\r".as_bytes())?;
    let completed = fresh
        .wait_screen("loopback turn committed")
        .map_err(|error| {
            let evidence = std::fs::read_to_string(&evidence).unwrap_or_default();
            format!("{error}\nphysical evidence:\n{evidence}")
        })?;
    assert!(completed.contains("hello from a real PTY"));
    assert!(
        completed.contains("✓ Loopback effect"),
        "completed live frame omitted the activity summary:\n{completed}"
    );
    let deadline = Instant::now() + WAIT;
    let id = loop {
        let sessions = product_sessions::list_sessions_for_cwd(&paths, &cwd)?;
        if let Some(projection) = sessions.first() {
            let session = product_sessions::load_session(&paths, &projection.id)?;
            if session.transcript.iter().any(|line| {
                line.text()
                    .is_some_and(|text| text.contains("loopback turn committed"))
            }) {
                break session.id;
            }
        }
        if Instant::now() >= deadline {
            let screen = fresh.screen();
            let evidence_text = std::fs::read_to_string(&evidence).unwrap_or_default();
            return Err(format!(
                "fresh turn did not commit to the product store\nscreen:\n{screen}\nevidence:\n{evidence_text}\nsessions: {sessions:#?}"
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let ready_session = wait_product_event(&paths, &cwd, "session.ready")?;
    assert_eq!(id, ready_session);
    let raw = fresh.quit_and_wait()?;
    assert!(raw.windows(7).any(|window| window == b"[?1000h"));
    assert!(raw.windows(7).any(|window| window == b"[?1000l"));
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));

    let sessions = product_sessions::list_sessions_for_cwd(&paths, &cwd)?;
    assert_eq!(sessions.len(), 1);
    let first = product_sessions::load_session(&paths, &sessions[0].id)?;
    assert!(first.plakous_locator.is_some());
    assert!(first.transcript.iter().any(|line| {
        line.text()
            .is_some_and(|text| text.contains("loopback turn committed"))
    }));
    let persisted_call_id = assert_completed_loopback_activity(&first)?;
    assert_eq!(first.id, id);
    let first_evidence = std::fs::read_to_string(&evidence)?;

    let mut resumed = PtyChild::spawn(&home, &cwd, &evidence, Some(&id))?;
    let restored = resumed.wait_screen("loopback turn committed")?;
    assert!(restored.contains("hello from a real PTY"));
    assert!(
        restored.contains("✓ Loopback effect"),
        "reopened frame omitted the persisted tool row:\n{restored}"
    );
    resumed.wait_screen("/10M")?;
    let resumed_raw = resumed.quit_and_wait()?;
    assert!(resumed_raw.windows(7).any(|window| window == b"[?1049l"));

    let reopened = product_sessions::load_session(&paths, &id)?;
    assert_eq!(reopened.id, id);
    assert!(reopened.plakous_locator.is_some());
    assert_eq!(reopened.transcript, first.transcript);
    assert_eq!(
        assert_completed_loopback_activity(&reopened)?,
        persisted_call_id
    );
    let final_evidence = std::fs::read_to_string(&evidence)?;
    assert_eq!(final_evidence, first_evidence);
    let physical = read_evidence(&evidence)?;
    assert_eq!(
        physical
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
            .count(),
        2
    );
    assert_eq!(
        physical
            .iter()
            .filter(|row| row.kind == EvidenceKind::EffectDispatched)
            .count(),
        1
    );
    let operation_ids = physical
        .iter()
        .filter_map(|row| row.operation_id.as_deref())
        .collect::<HashSet<_>>();
    assert_eq!(
        operation_ids.len(),
        3,
        "physical operation ids must not repeat"
    );
    Ok(())
}

fn account_injection(
    workspace: &Path,
    controller: &BoundaryController,
    primary: &'static [u8],
    target: &'static [u8],
) -> Result<Arc<ScriptedRuntimeFactory>, Box<dyn Error + Send + Sync>> {
    let rebind_target =
        runtime_binding_for_account(workspace, controller.clone(), target)?.target_binding();
    let workspace = workspace.to_path_buf();
    let controller = controller.clone();
    Ok(Arc::new(ScriptedRuntimeFactory {
        runtime: Arc::new(move |role| {
            let account = if role == RuntimeRole::Rebind {
                target
            } else {
                primary
            };
            runtime_binding_for_account(&workspace, controller.clone(), account)
                .map_err(|error| error.to_string())
        }),
        rebind_target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: Mutex::new(Vec::new()),
    }))
}

fn host_config(
    paths: &PlakPaths,
    workspace: &Path,
    session: SessionRecord,
) -> HostConfig<crate::surface_protocol::PlakExtension> {
    HostConfig {
        surface: Some(crate::shell::settings::ProductSurface::boxed(paths)),
        auto_continue: true,
        topology_admission: None,
        state_roots: crate::shell::host::product_state_roots(paths),
        product_home: paths.home().to_path_buf(),
        home: paths.session_home(),
        store: Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        )),
        session,
        target: plakous_hosted::runtime::test_kit::registered_authority(NativeProvider::Codex),
        model_id: "offline-loopback-v1".to_owned(),
        effort: Some(ReasoningEffort::Medium),
        effort_explicit: true,
        compaction_enabled: false,
        max_model_rounds: crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_enabled: crate::product_config::SessionPolicy::load(paths)
            .unwrap()
            .topology_enabled,
        root_template: None,
        attached_one_shot: false,
        interactive: true,
        cwd: workspace.to_path_buf(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        resume_publication: None,
        runtime_factory: Arc::new(plakous_hosted::runtime::ProductionRuntimeFactory),
        ready_path: crate::product_host::ReadyPathTiming::default(),
        seed_history: None,
        source_releases: crate::product_host::SourceReleases::default(),
    }
}

fn dual_binding_ready_catalog() -> plakous_host_protocol::views::ReadyCatalog {
    [
        ("binding-a", "binding-a-model", "medium"),
        ("binding-b", "binding-b-model", "low"),
    ]
    .into_iter()
    .map(
        |(account, model, effort)| plakous_host_protocol::views::ProviderSection {
            account: account.to_owned(),
            label: account.to_owned(),
            ready: true,
            entries: vec![plakous_host_protocol::views::ModelEntry {
                id: model.to_owned(),
                tag: "offline exact binding".to_owned(),
                reasoning: true,
                effort_options: vec![effort.to_owned()],
                extras: Vec::new(),
                unavailable_reason: None,
            }],
        },
    )
    .collect::<Vec<_>>()
    .into()
}

fn long_work_ready_catalog() -> plakous_host_protocol::views::ReadyCatalog {
    vec![plakous_host_protocol::views::ProviderSection {
        account: "codex".to_owned(),
        label: "Codex".to_owned(),
        ready: true,
        entries: vec![
            plakous_host_protocol::views::ModelEntry {
                id: "offline-loopback-v1".to_owned(),
                tag: "offline source binding".to_owned(),
                reasoning: true,
                effort_options: vec!["medium".to_owned()],
                extras: Vec::new(),
                unavailable_reason: None,
            },
            plakous_host_protocol::views::ModelEntry {
                id: "offline-loopback-v2".to_owned(),
                tag: "offline target binding".to_owned(),
                reasoning: true,
                effort_options: vec!["low".to_owned()],
                extras: Vec::new(),
                unavailable_reason: None,
            },
        ],
    }]
    .into()
}

fn wait_for_event(
    events: &crate::surface_protocol::HostEventReceiver,
    observed: &mut Vec<crate::surface_protocol::HostEvent>,
    done: impl Fn(&crate::surface_protocol::HostEvent) -> bool,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for host event: {observed:#?}").into());
        }
        let event = events.recv_timeout(remaining).map_err(|error| {
            std::io::Error::other(format!(
                "host event wait failed ({error:?}); observed: {observed:#?}"
            ))
        })?;
        let complete = done(&event);
        observed.push(event);
        if complete {
            return Ok(());
        }
    }
}

fn send_action(
    actions: &crate::surface_protocol::UserActionSender,
    action: crate::surface_protocol::UserAction,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    actions.send(action).map_err(|error| {
        std::io::Error::other(format!(
            "host action queue refused qualification input: {error:?}"
        ))
        .into()
    })
}

#[test]
fn exact_resume_is_published_only_by_successfully_attached_runtime_host()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&workspace)?;
    let evidence = root.path().join("resume-attachment.jsonl");
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
    let paths = PlakPaths::explicit(home);
    let mut session = SessionRecord::new(workspace.clone());
    session.account = Some("binding-a".to_owned());
    session.model = Some("binding-a-model".to_owned());
    session.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut session)?;
    let id = session.id.clone();
    let revision = session.store_revision;
    let publication = Arc::new(AtomicBool::new(false));

    let mut config = host_config(&paths, &workspace, session);
    config.runtime_factory = account_injection(
        &workspace,
        &controller,
        b"binding-a-account",
        b"binding-b-account",
    )?;
    config.resume_publication = Some(Arc::clone(&publication));
    let (actions, _events, worker) = crate::product_host::spawn_host_worker(config)?;

    assert!(publication.load(Ordering::Acquire));
    assert!(product_sessions::load_session(&paths, &id)?.store_revision > revision);
    assert!(
        crate::product_session_log::read_tail(&paths.session_home(), &id, 16)?
            .iter()
            .any(|line| line.contains("session.resumed"))
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "resume host panicked")?
        .map_err(|error| error.message().to_owned())?;
    Ok(())
}

/// A profile Fork now carries the conversation onto the profile's root: the
/// history and the queue drained under the source continue on the branch, and
/// a second Fork now back onto the first root keeps every exact branch.
#[test]
fn injected_dual_profile_fork_continues_history_queue_and_reverse_switch()
-> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"binding-a-account";
    const B: &[u8] = b"binding-b-account";
    const A_RETURN: &[u8] = b"binding-a-return-account";
    let (root, paths, workspace) = profile_home("low")?;
    std::fs::write(
        paths.home().join("agents/pa-root.toml"),
        "account = \"codex\"\nmodel = \"offline-loopback-v1\"\neffort = \"medium\"\ndescription = \"root\"\n",
    )?;
    std::fs::write(
        paths.home().join("profiles/pa.toml"),
        "root = \"pa-root\"\nworking_set = [\"pb-worker\"]\n",
    )?;
    let evidence = root.path().join("dual-binding.jsonl");
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut source)?;
    let source_id = source.id.clone();
    let fork_onto = |name: &str| {
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: name.to_owned(),
                fork_now: true,
            },
        )
    };

    let mut first_config = host_config(&paths, &workspace, source);
    first_config.ready_catalog = profile_ready_catalog();
    first_config.runtime_factory = account_injection(&workspace, &controller, A, B)?;
    let (actions, events, worker) = crate::product_host::spawn_host_worker(first_config)?;
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("binding A first turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::SetBusy(true))
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::FollowUp(
            "queued while binding A is busy".to_owned().into(),
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, fork_onto("pb"))?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().rev().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("binding B continued turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "binding B host panicked")?
        .map_err(|error| error.message().to_owned())?;

    let after_b = product_sessions::list_sessions_for_cwd(&paths, &workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(&paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(after_b.len(), 2, "A→B must create one branch: {after_b:#?}");
    let branch_b = after_b
        .iter()
        .find(|row| row.model.as_deref() == Some("binding-b-model"))
        .ok_or("binding B branch missing")?;
    assert_ne!(branch_b.id, source_id);
    assert_eq!(
        branch_b.pending_queue,
        [] as [plakous_hosted::sessions::QueueItem; 0]
    );
    assert_eq!(
        branch_b.prompt_history,
        [
            "binding A first turn",
            "queued while binding A is busy",
            "binding B continued turn"
        ]
    );
    for prompt in &branch_b.prompt_history {
        assert!(
            branch_b
                .transcript
                .iter()
                .any(|line| line.text().is_some_and(|text| text.contains(prompt)))
        );
    }
    let branch_b_id = branch_b.id.clone();

    let mut reverse_config = host_config(
        &paths,
        &workspace,
        product_sessions::load_session(&paths, &branch_b_id)?,
    );
    reverse_config.ready_catalog = profile_ready_catalog();
    // `host_config` fixes the source tuple; this host reopens the B branch.
    reverse_config.model_id = "binding-b-model".to_owned();
    reverse_config.effort = Some(ReasoningEffort::Low);
    let reverse_events = drive_injected_binding_host(
        reverse_config,
        account_injection(&workspace, &controller, B, A_RETURN)?,
        vec![
            (
                fork_onto("pa"),
                Box::new(|event| profile_outcome(event).is_some()),
            ),
            (
                crate::surface_protocol::UserAction::Prompt(
                    "binding A resumed after reverse switch".to_owned().into(),
                ),
                Box::new(|event| {
                    matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
                }),
            ),
        ],
    )?;
    assert_eq!(
        reverse_events.iter().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{reverse_events:#?}"
    );
    assert!(reverse_events.iter().any(|event| matches!(event,
        crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
            if note.contains("Model switched") && note.contains("available to resume")
    )));
    let after_a = product_sessions::list_sessions_for_cwd(&paths, &workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(&paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(after_a.len(), 3, "A→B→A must retain all exact branches");
    let final_a = after_a
        .iter()
        .find(|row| {
            row.id != source_id
                && row.id != branch_b_id
                && row.model.as_deref() == Some("offline-loopback-v1")
        })
        .ok_or("reverse binding A branch missing")?;
    assert_eq!(final_a.prompt_history.len(), 4);
    assert_eq!(
        final_a.prompt_history.last().map(String::as_str),
        Some("binding A resumed after reverse switch")
    );
    assert_eq!(
        final_a.pending_queue,
        [] as [plakous_hosted::sessions::QueueItem; 0]
    );
    let physical = read_evidence(&evidence)?;
    assert_eq!(
        physical
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
            .count(),
        7,
        "four root turns plus each of three binding incarnations' first-turn tool continuation"
    );
    Ok(())
}

/// Fork now from a Fast (`NativePriority`) root onto a non-fast profile lands
/// `Native`, and the reverse onto a `fast_mode = true` root lands
/// `NativePriority`.
#[test]
fn profile_fork_maps_the_fast_lane_on_and_off() -> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"fast-source-account";
    const B: &[u8] = b"standard-target-account";
    const FAST: &[u8] = b"fast-return-account";
    let (root, paths, workspace) = profile_home("low")?;
    std::fs::write(
        paths.home().join("agents/pa-root.toml"),
        "account = \"codex\"\nmodel = \"offline-loopback-v1\"\neffort = \"medium\"\nfast_mode = true\ndescription = \"fast root\"\n",
    )?;
    std::fs::write(
        paths.home().join("profiles/pa.toml"),
        "root = \"pa-root\"\nworking_set = [\"pb-worker\"]\n",
    )?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("fast-lane.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    source.target_selection = plakous_hosted::binding::TargetSelection::NativePriority;
    product_sessions::save_session(&paths, &mut source)?;
    let source_id = source.id.clone();
    let fork_onto = |name: &str| {
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: name.to_owned(),
                fork_now: true,
            },
        )
    };

    let mut first = host_config(&paths, &workspace, source);
    first.ready_catalog = profile_ready_catalog();
    first.runtime_factory = account_injection(&workspace, &controller, A, B)?;
    let (actions, events, worker) = crate::product_host::spawn_host_worker(first)?;
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("fast source turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, fork_onto("pb"))?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().rev().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "fast-lane host panicked")?
        .map_err(|error| error.message().to_owned())?;

    let rows = product_sessions::list_sessions_for_cwd(&paths, &workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(&paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    let kept = rows
        .iter()
        .find(|row| row.id == source_id)
        .ok_or("fast source missing")?;
    assert_eq!(
        kept.target_selection,
        plakous_hosted::binding::TargetSelection::NativePriority
    );
    let standard = rows
        .iter()
        .find(|row| row.id != source_id)
        .ok_or("non-fast branch missing")?;
    assert_eq!(
        standard.target_selection,
        plakous_hosted::binding::TargetSelection::Native
    );
    let standard_id = standard.id.clone();

    let mut reverse = host_config(
        &paths,
        &workspace,
        product_sessions::load_session(&paths, &standard_id)?,
    );
    reverse.ready_catalog = profile_ready_catalog();
    reverse.model_id = "binding-b-model".to_owned();
    reverse.effort = Some(ReasoningEffort::Low);
    reverse.runtime_factory = account_injection(&workspace, &controller, B, FAST)?;
    let (actions, events, worker) = crate::product_host::spawn_host_worker(reverse)?;
    let mut observed = Vec::new();
    send_action(&actions, fork_onto("pa"))?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "fast return host panicked")?
        .map_err(|error| error.message().to_owned())?;

    let fast = product_sessions::list_sessions_for_cwd(&paths, &workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(&paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|row| row.id != source_id && row.id != standard_id)
        .ok_or("fast branch missing")?;
    assert_eq!(
        fast.target_selection,
        plakous_hosted::binding::TargetSelection::NativePriority
    );
    assert_eq!(fast.model.as_deref(), Some("offline-loopback-v1"));
    Ok(())
}

/// A native-ptc prefix forks onto a Direct profile root, and the next turn
/// commits on that composition.
#[test]
fn profile_fork_from_native_ptc_onto_direct_commits_a_turn()
-> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"native-ptc-source";
    const B: &[u8] = b"direct-profile-root";
    let (root, paths, workspace) = profile_home("low")?;
    std::fs::write(
        paths.home().join("agents/pb-root.toml"),
        "account = \"codex\"\nmodel = \"binding-b-model\"\neffort = \"low\"\ncomposition = \"direct\"\ndescription = \"direct root\"\n",
    )?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("code-mode-fork.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    source.composition = plakous_hosted::binding::CompositionSelection::NativePtc;
    product_sessions::save_session(&paths, &mut source)?;
    let source_id = source.id.clone();
    let mut config = host_config(&paths, &workspace, source);
    config.ready_catalog = profile_ready_catalog();
    config.runtime_factory = account_injection(&workspace, &controller, A, B)?;
    let (actions, events, worker) = crate::product_host::spawn_host_worker(config)?;
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("code mode prefix".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: "pb".to_owned(),
                fork_now: true,
            },
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().rev().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("direct turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "code-mode fork host panicked")?
        .map_err(|error| error.message().to_owned())?;

    let rows = product_sessions::list_sessions_for_cwd(&paths, &workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(&paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    let kept = rows
        .iter()
        .find(|row| row.id == source_id)
        .ok_or("native-ptc source missing")?;
    assert_eq!(
        kept.composition,
        plakous_hosted::binding::CompositionSelection::NativePtc
    );
    let target = rows
        .iter()
        .find(|row| row.id != source_id)
        .ok_or("direct branch missing")?;
    assert_eq!(
        target.composition,
        plakous_hosted::binding::CompositionSelection::Direct
    );
    assert!(
        target
            .transcript
            .iter()
            .any(|line| line.text().is_some_and(|text| text.contains("direct turn")))
    );
    assert!(target.transcript.iter().any(|line| {
        line.text()
            .is_some_and(|text| text.contains("loopback turn committed"))
    }));
    Ok(())
}

type BindingHostStep = (
    crate::surface_protocol::UserAction,
    Box<dyn Fn(&crate::surface_protocol::HostEvent) -> bool>,
);

fn drive_injected_binding_host(
    mut config: HostConfig<crate::surface_protocol::PlakExtension>,
    injection: Arc<ScriptedRuntimeFactory>,
    steps: Vec<BindingHostStep>,
) -> Result<Vec<crate::surface_protocol::HostEvent>, Box<dyn Error + Send + Sync>> {
    config.runtime_factory = injection;
    let (actions, events, worker) = crate::product_host::spawn_host_worker(config)?;
    let mut observed = Vec::new();
    for (action, completed) in steps {
        send_action(&actions, action)?;
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(
                    format!("timed out waiting for binding host event: {observed:#?}").into(),
                );
            }
            let event = events.recv_timeout(remaining).map_err(|error| {
                std::io::Error::other(format!(
                    "binding host event wait failed ({error:?}): {observed:#?}"
                ))
            })?;
            let done = completed(&event);
            observed.push(event);
            if done {
                break;
            }
        }
    }
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| std::io::Error::other("injected binding host panicked"))?
        .map_err(|error| error.message().to_owned())?;
    observed.extend(events.try_iter());
    Ok(observed)
}

// ---------------------------------------------------------------------------
// §6.1 interrupt ladder and §4 composer editing, driven as literal control
// bytes through the real PTY. These cases exist because unit-level `KeyEvent`
// coverage cannot prove that a terminal's control byte, CSI modifier sequence,
// or bracketed-paste envelope reaches the handler it is specified to reach.
// ---------------------------------------------------------------------------

/// The Activity body reports why the offline composition has no agent tree.
const AGENTS_UNAVAILABLE: &str = "Sub-agents are unavailable:";

/// A tool *summary* row: it names the subject and carries the header glyph
/// (`✓`/`◌`/`×`/`⊘`/`›` on Direct, `▶`/`▼` on Compose). The expanded detail
/// rail repeats the subject on `│`/`└` lines, so those rails are excluded.
fn tool_summary_row(line: &str, subject: &str) -> bool {
    if !line.contains(subject) {
        return false;
    }
    let header = line.contains('\u{25b6}')
        || line.contains('\u{25bc}')
        || line.contains('✓')
        || line.contains('◌')
        || line.contains('×')
        || line.contains('⊘')
        || line.contains('›');
    header && !line.contains("│ ") && !line.contains("└ ")
}

/// Spawn the offline TUI on a fresh isolated home and wait for the ready shell.
///
/// With `stream_gate`, the first provider response stream parks until that file
/// appears, leaving a real streaming turn for the interrupt cases.
fn spawn_ready(
    root: &Path,
    gate: Option<TurnGate<'_>>,
) -> Result<(PtyChild, PathBuf, PlakPaths, PathBuf), Box<dyn Error>> {
    let home = root.join("home");
    let cwd = root.join("workspace");
    let evidence = root.join("physical.jsonl");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&cwd)?;
    let paths = PlakPaths::explicit(home.clone());
    let mut child = match gate {
        Some(gate) => PtyChild::spawn_gated(&home, &cwd, &evidence, gate)?,
        None => PtyChild::spawn(&home, &cwd, &evidence, None)?,
    };
    if let Err(error) = child.wait_screen("/10M") {
        let attach = std::fs::read_to_string(home.join("pty-attach-error.txt"))
            .unwrap_or_else(|_| "no attach diagnostic was recorded".to_owned());
        return Err(format!("{error}; attach: {attach}").into());
    }
    child.wait_screen("/ commands")?;
    Ok((child, cwd, paths, evidence))
}

/// Park the TUI on a streaming turn behind the response-stream gate.
fn drive_to_streaming_turn(
    child: &mut PtyChild,
    evidence: &Path,
    prompt: &str,
) -> Result<(), Box<dyn Error>> {
    child.send(format!("{prompt}\r").as_bytes())?;
    wait_physical_evidence(evidence, EvidenceKind::ProviderOutcome, 1)?;
    child.wait_screen("waiting for model")?;
    Ok(())
}

/// Assert the settled interrupt projection: a kept user block, the `⊘ interrupted`
/// marker, and no recovery-required escalation (§6.1).
fn assert_interrupted_frame(frame: &str, prompt: &str) {
    assert!(
        !frame.contains("recovery required") && !frame.contains("requires recovery"),
        "an observed cancel must never be projected as recovery-required:\n{frame}"
    );
    assert!(
        frame.contains(prompt),
        "the cancelled root must keep its user block:\n{frame}"
    );
}

/// §6.1 Ctrl+C ladder rung 1 then rung 2, sent as the literal byte `0x03`.
#[test]
fn real_pty_ctrl_c_clears_draft_then_cancels_a_streaming_turn() -> Result<(), Box<dyn Error>> {
    const DRAFT: &str = "ctrlc must clear this draft";
    const PROMPT: &str = "ctrlc cancels this streaming root";
    const AFTER: &str = "prompt after the ctrlc cancel";

    let root = PlakPaths::isolated_tempdir();
    let release = root.path().join("release-gated-stream");
    let (mut child, cwd, paths, evidence) =
        spawn_ready(root.path(), Some(TurnGate::Stream(&release)))?;

    // Rung 1: a non-empty draft is cleared and nothing else happens.
    child.type_draft(DRAFT)?;
    child.send(b"\x03")?;
    child.wait_draft_absent(DRAFT)?;
    assert!(
        child.screen().contains("/ commands"),
        "Ctrl+C on a draft must not disturb the shell:\n{}",
        child.screen()
    );

    // Rung 2: an empty draft with a turn streaming asks in the turn stop
    // pop-up. Focus starts on Stop turn, so Enter cancels it.
    drive_to_streaming_turn(&mut child, &evidence, PROMPT)?;
    child.send(b"\x03")?;
    child.wait_screen("Stop this turn?")?;
    child.send(b"\r")?;
    let interrupted = child.wait_screen("⊘ interrupted").map_err(|error| {
        let evidence = std::fs::read_to_string(&evidence).unwrap_or_default();
        format!("{error}\nphysical evidence:\n{evidence}")
    })?;
    assert_interrupted_frame(&interrupted, PROMPT);

    // The conversation stays open for the next prompt (§6.1 "continuable").
    std::fs::write(&release, b"release")?;
    child.send(format!("{AFTER}\r").as_bytes())?;
    let continued = child.wait_screen("loopback turn committed")?;
    assert!(continued.contains(AFTER), "{continued}");

    let raw = child.quit_and_wait()?;
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));

    let session = product_sessions::list_sessions_for_cwd(&paths, &cwd)?
        .first()
        .map(|projection| product_sessions::load_session(&paths, &projection.id))
        .transpose()?
        .ok_or("the interrupt journey left no product session")?;
    assert!(
        session.transcript.contains(&TranscriptRow::Interrupted),
        "the interrupted turn must persist its marker: {:?}",
        session.transcript
    );
    assert!(
        session.transcript.iter().any(|line| line
            .text()
            .is_some_and(|text| text.contains("loopback turn committed"))),
        "the post-interrupt root must commit: {:?}",
        session.transcript
    );
    Ok(())
}

/// §6.1 Esc: mid-turn Esc cancels; idle Esc-Esc clears a draft that one Ctrl+Z
/// then restores (§4 bounded undo).
#[test]
fn real_pty_esc_cancels_a_streaming_turn_and_esc_esc_clears_an_undoable_draft()
-> Result<(), Box<dyn Error>> {
    const PROMPT: &str = "esc cancels this streaming root";
    const DRAFT: &str = "escesc must clear this recoverable draft";

    let root = PlakPaths::isolated_tempdir();
    let release = root.path().join("release-gated-stream");
    let (mut child, _cwd, _paths, evidence) =
        spawn_ready(root.path(), Some(TurnGate::Stream(&release)))?;

    drive_to_streaming_turn(&mut child, &evidence, PROMPT)?;
    child.send(b"\x1b")?;
    child.wait_screen("Stop this turn?")?;
    // Focus starts on Stop turn, so Enter cancels it.
    child.send(b"\r")?;
    let interrupted = child.wait_screen("⊘ interrupted").map_err(|error| {
        let evidence = std::fs::read_to_string(&evidence).unwrap_or_default();
        format!("{error}\nphysical evidence:\n{evidence}")
    })?;
    assert_interrupted_frame(&interrupted, PROMPT);
    // Esc never quits at any level (§6.1).
    assert!(
        child.child.try_wait()?.is_none(),
        "Esc must never quit the TUI"
    );
    std::fs::write(&release, b"release")?;

    child.type_draft(DRAFT)?;
    child.esc_esc_clear_draft(DRAFT)?;
    child.send(b"\x1a")?;
    let restored = child.wait_stable_where("Ctrl+Z restored the cleared draft", |screen| {
        screen.contains(DRAFT)
    })?;
    // The restored draft is a draft, not a submitted turn.
    assert!(
        !restored.contains("loopback turn committed"),
        "undo must not submit the restored draft:\n{restored}"
    );

    child.esc_esc_clear_draft(DRAFT)?;
    let raw = child.quit_and_wait()?;
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));
    Ok(())
}

/// §6.1 Esc while a mutating tool is *running*: the effect joins the cancel with
/// its own classification, the turn terminalises `Cancelled` (never
/// recovery-required), and the next prompt commits in the same
/// session. This is the Direct-tool half of the cell-cancel law; a Code Mode
/// cell reaches the same runtime through its nested commands.
#[test]
fn real_pty_esc_cancels_a_running_mutating_tool_and_the_next_prompt_commits()
-> Result<(), Box<dyn Error>> {
    const PROMPT: &str = "esc cancels this running tool";
    const AFTER: &str = "prompt after the running tool cancel";

    let root = PlakPaths::isolated_tempdir();
    let release = root.path().join("release-gated-effect");
    let (mut child, cwd, paths, evidence) =
        spawn_ready(root.path(), Some(TurnGate::Effect(&release)))?;

    child.send(format!("{PROMPT}\r").as_bytes())?;
    wait_physical_evidence(&evidence, EvidenceKind::EffectDispatched, 1)?;
    // Esc is a cancel only once the TUI itself shows the running tool; the
    // physical evidence lands before the host's busy projection has painted.
    child.wait_screen("Loopback effect")?;
    child.send(b"\x1b")?;
    child.wait_screen("Stop this turn?")?;
    // Focus starts on Stop turn, so Enter cancels it.
    child.send(b"\r")?;
    let interrupted = child.wait_screen("⊘ interrupted").map_err(|error| {
        let evidence = std::fs::read_to_string(&evidence).unwrap_or_default();
        format!("{error}\nphysical evidence:\n{evidence}")
    })?;
    assert_interrupted_frame(&interrupted, PROMPT);
    assert!(
        !read_evidence(&evidence)?
            .iter()
            .any(|row| row.kind == EvidenceKind::EffectOutcome),
        "the joined effect must not fabricate an outcome after the cancel"
    );

    std::fs::write(&release, b"release")?;
    child.send(format!("{AFTER}\r").as_bytes())?;
    let continued = child.wait_screen("loopback turn committed")?;
    assert!(continued.contains(AFTER), "{continued}");

    let raw = child.quit_and_wait()?;
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));

    let session = product_sessions::list_sessions_for_cwd(&paths, &cwd)?
        .first()
        .map(|projection| product_sessions::load_session(&paths, &projection.id))
        .transpose()?
        .ok_or("the running-tool interrupt left no product session")?;
    assert!(
        session.transcript.contains(&TranscriptRow::Interrupted),
        "the interrupted turn must persist its marker: {:?}",
        session.transcript
    );
    assert!(
        !session.transcript.iter().any(|line| line
            .text()
            .is_some_and(|text| text.contains("requires recovery"))
            || line
                .text()
                .is_some_and(|text| text.contains("recovery required"))),
        "a joined cancel must never be persisted as recovery-required: {:?}",
        session.transcript
    );
    Ok(())
}

/// §5.3 quit while a mutating tool is *running*: the terminal is restored
/// before the host is asked to cancel, the process exits without waiting out
/// session release, and the saved session resumes at once for another turn.
#[test]
fn real_pty_ctrl_q_during_a_running_mutating_tool_releases_and_reopens()
-> Result<(), Box<dyn Error>> {
    const PROMPT: &str = "quit during this running mutating tool";
    const AFTER: &str = "turn after the running-tool quit";

    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let release = root.path().join("release-gated-effect");
    let (mut child, cwd, paths, evidence) =
        spawn_ready(root.path(), Some(TurnGate::Effect(&release)))?;

    child.send(format!("{PROMPT}\r").as_bytes())?;
    wait_physical_evidence(&evidence, EvidenceKind::EffectDispatched, 1)?;
    // The effect is parked inside its dispatch: the TUI shows a genuinely
    // running mutating tool when the quit byte lands.
    child.wait_screen("Loopback effect")?;

    // Mid-turn, quit asks first. Focus starts on Quit, so Enter confirms.
    child.send(b"\x11")?;
    child.wait_screen("Quit plak?")?;
    child.send(b"\r")?;
    child.wait_raw_contains(b"\x1b[?1049l")?;
    let raw = child.wait_exit()?;
    let leave = position(&raw, b"\x1b[?1049l").expect("Ctrl+Q must leave the alternate screen");
    assert!(
        position(&raw[leave..], b"\x1b[?25h").is_some(),
        "the cursor must be shown after the alternate screen is left"
    );
    assert!(
        !read_evidence(&evidence)?
            .iter()
            .any(|row| row.kind == EvidenceKind::EffectOutcome),
        "a quit-joined effect must not fabricate an outcome after the cancel"
    );

    let saved = product_sessions::list_sessions_for_cwd(&paths, &cwd)?;
    let id = saved
        .iter()
        .find(|projection| {
            product_sessions::load_session(&paths, &projection.id)
                .is_ok_and(|session| {
                    session.transcript.iter().any(|row| {
                        matches!(row, TranscriptRow::User { text, sent_at_ms: Some(_) } if text == PROMPT)
                    })
                })
        })
        .map(|projection| projection.id.clone())
        .ok_or("the running-tool quit saved no session with its cancelled turn")?;
    let session_count = saved.len();

    // Immediate resume of that session: it attaches and performs another turn.
    let reopen_evidence = root.path().join("physical-reopen.jsonl");
    let mut child = PtyChild::spawn(&home, &cwd, &reopen_evidence, Some(id.as_str()))?;
    child.wait_screen("/ commands")?;
    child.send(format!("{AFTER}\r").as_bytes())?;
    let continued = child.wait_screen("loopback turn committed")?;
    assert!(continued.contains(AFTER), "{continued}");
    assert!(
        !continued.contains("recovery required"),
        "the resumed session must not demand reconciliation after a joined quit:\n{continued}"
    );

    let raw = child.quit_and_wait()?;
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));

    assert_eq!(
        product_sessions::list_sessions_for_cwd(&paths, &cwd)?.len(),
        session_count,
        "resume must attach the saved session, not create a fresh one"
    );
    let session = product_sessions::load_session(&paths, &id)?;
    // Operator prompts carry their send time; the rows are matched without it.
    let without_send_time = |row: &TranscriptRow| match row {
        TranscriptRow::User { text, .. } => TranscriptRow::user(text.clone()),
        other => other.clone(),
    };
    for expected in [
        TranscriptRow::user(PROMPT),
        TranscriptRow::Interrupted,
        TranscriptRow::user(AFTER),
    ] {
        assert!(
            session
                .transcript
                .iter()
                .any(|line| without_send_time(line) == expected),
            "the resumed session must keep `{expected:?}`: {:?}",
            session.transcript
        );
    }
    Ok(())
}

/// §6.1 "Quit is `/quit` or `Ctrl-Q`": the literal byte `0x11` exits cleanly and
/// the terminal is handed back the way it was borrowed.
#[test]
fn real_pty_ctrl_q_exits_cleanly_and_restores_the_terminal() -> Result<(), Box<dyn Error>> {
    let root = PlakPaths::isolated_tempdir();
    let (mut child, _cwd, _paths, _evidence) = spawn_ready(root.path(), None)?;

    // A mouse moving over the quitting screen: SGR motion reports arrive right
    // behind the quit byte, while the UI is no longer reading. They must be
    // discarded with mouse reporting, never handed on once raw mode ends.
    let mut quit = b"\x11".to_vec();
    for _ in 0..64 {
        quit.extend_from_slice(b"\x1b[<35;40;12M");
    }
    child.send(&quit)?;
    let raw = child.wait_exit()?;
    assert_terminal_handed_back_cleanly(&raw);
    Ok(())
}

/// §5.3: `SIGTERM` and `SIGHUP` quit the TUI the way `Ctrl+Q` does. The
/// terminal is handed back in the ordinary order, the process reports the
/// signalled status, and the session it was in reopens with its turn.
#[test]
fn real_pty_terminating_signals_restore_the_terminal_and_leave_a_resumable_session()
-> Result<(), Box<dyn Error>> {
    use rustix::process::Signal;
    for (signal, name) in [(Signal::TERM, "SIGTERM"), (Signal::HUP, "SIGHUP")] {
        let root = PlakPaths::isolated_tempdir();
        let (mut child, cwd, paths, evidence) = spawn_ready(root.path(), None)?;
        let prompt = format!("a turn before {name}");
        child.send(format!("{prompt}\r").as_bytes())?;
        child.wait_screen("loopback turn committed")?;

        child.signal(signal)?;
        let (status, raw) = child.wait_exit_status()?;
        assert_eq!(
            status.exit_code(),
            u32::try_from(crate::signals::SIGNALLED_EXIT_CODE)?,
            "{name}: {status:?}"
        );
        assert_terminal_handed_back_cleanly(&raw);
        let leave = position(&raw, b"\x1b[?1049l").expect("leave");
        assert!(
            String::from_utf8_lossy(&raw[leave..])
                .contains("Resume this session with: plak resume "),
            "{name}: the cooked terminal gets the resume hint"
        );

        let sessions = product_sessions::list_sessions_for_cwd(&paths, &cwd)?;
        assert_eq!(sessions.len(), 1, "{name}");
        let id = sessions[0].id.clone();
        let mut resumed = PtyChild::spawn(paths.home(), &cwd, &evidence, Some(&id))?;
        let reopened = resumed.wait_screen("loopback turn committed")?;
        assert!(reopened.contains(&prompt), "{name}:\n{reopened}");
        resumed.wait_screen("/ commands")?;
        let resumed_raw = resumed.quit_and_wait()?;
        assert!(position(&resumed_raw, b"\x1b[?1049l").is_some(), "{name}");
    }
    Ok(())
}

/// §5.3: a second terminating signal while the first quit is in flight still
/// exits with the terminal handed back, whichever restore wins.
#[test]
fn real_pty_a_second_terminating_signal_still_restores_the_terminal() -> Result<(), Box<dyn Error>>
{
    use rustix::process::Signal;
    let root = PlakPaths::isolated_tempdir();
    let (child, _cwd, _paths, _evidence) = spawn_ready(root.path(), None)?;
    child.signal(Signal::TERM)?;
    child.signal(Signal::HUP)?;
    let (status, raw) = child.wait_exit_status()?;
    assert_eq!(
        status.exit_code(),
        u32::try_from(crate::signals::SIGNALLED_EXIT_CODE)?,
        "{status:?}"
    );
    let leave = position(&raw, b"\x1b[?1049l").expect("the alternate screen is left");
    for mode in [&b"\x1b[?1000l"[..], b"\x1b[?2004l"] {
        assert!(
            position(&raw, mode).is_some_and(|at| at < leave),
            "{:?} must precede the leave",
            String::from_utf8_lossy(mode)
        );
    }
    assert!(position(&raw[leave..], b"\x1b[?25h").is_some());
    Ok(())
}

fn position(raw: &[u8], needle: &[u8]) -> Option<usize> {
    raw.windows(needle.len())
        .position(|window| window == needle)
}

/// The restore order §6 requires: mouse reporting and bracketed paste off
/// before the alternate screen is left (raw mode ends inside that leave), the
/// cursor shown after it, and no mouse report in anything written past it.
fn assert_terminal_handed_back_cleanly(raw: &[u8]) {
    let mouse_off = position(raw, b"\x1b[?1000l").expect("Ctrl+Q must disable mouse reporting");
    let paste_off = position(raw, b"\x1b[?2004l").expect("Ctrl+Q must disable bracketed paste");
    let leave = position(raw, b"\x1b[?1049l").expect("Ctrl+Q must leave the alternate screen");
    assert!(
        mouse_off < leave && paste_off < leave,
        "mouse ({mouse_off}) and paste ({paste_off}) must be released before the alternate screen is left ({leave})"
    );
    let after_leave = &raw[leave..];
    assert!(
        position(after_leave, b"\x1b[?25h").is_some(),
        "the cursor must be shown after the alternate screen is left"
    );
    assert!(
        position(after_leave, b"\x1b[<").is_none(),
        "no mouse report may follow the alternate-screen leave: {:?}",
        String::from_utf8_lossy(after_leave)
    );
    let cooked = String::from_utf8_lossy(after_leave);
    assert!(
        !cooked.contains("releasing the session") && !cooked.contains("exceeded its"),
        "quit must not hold the cooked terminal on host release:\n{cooked}"
    );
}

/// §6: after a committed turn the quit hands the terminal back at once and the
/// cooked terminal gets the resume hint; the process then exits instead of
/// joining host release, so neither the alternate screen nor the shell prompt
/// outlives the UI. Both waits are timed under the load-aware budget.
#[test]
fn real_pty_ctrl_q_after_a_turn_restores_first_then_prints_the_resume_hint()
-> Result<(), Box<dyn Error>> {
    const HINT_PREFIX: &str = "Resume this session with: plak resume ";
    let root = PlakPaths::isolated_tempdir();
    let (mut child, cwd, _paths, _evidence) = spawn_ready(root.path(), None)?;
    child.send(b"quit right after this loopback turn\r")?;
    child.wait_screen("loopback turn committed")?;
    // After the turn: idle, so the quit is immediate.
    child.wait_screen("last user prompt")?;

    let started = Instant::now();
    child.send(b"\x11")?;
    child.wait_raw_contains(b"\x1b[?1049l")?;
    let handed_back = started.elapsed();
    let raw = child.wait_exit()?;
    let exited = started.elapsed();

    let restore_budget = latency_budget(Duration::from_millis(500));
    assert!(
        handed_back <= restore_budget,
        "the terminal was handed back after {handed_back:?}, over the {restore_budget:?} budget"
    );
    let exit_budget = latency_budget(Duration::from_millis(500));
    assert!(
        exited <= exit_budget,
        "Ctrl+Q after a turn took {exited:?}, over the {exit_budget:?} budget"
    );
    assert_terminal_handed_back_cleanly(&raw);

    let leave = position(&raw, b"\x1b[?1049l").expect("leave");
    let cooked = String::from_utf8_lossy(&raw[leave..]);
    let hint = cooked
        .lines()
        .find_map(|line| line.find(HINT_PREFIX).map(|at| &line[at..]))
        .unwrap_or_else(|| panic!("no resume hint on the cooked terminal:\n{cooked}"));
    let short_id = hint[HINT_PREFIX.len()..]
        .split_whitespace()
        .next()
        .expect("short id");
    assert_eq!(short_id.len(), crate::tui::SHORT_ID_CHARS, "{hint}");
    assert!(
        short_id.chars().all(|c| c.is_ascii_hexdigit()),
        "the short id is a uuid prefix: {hint}"
    );
    assert!(
        hint.contains(&format!("(workspace {})", cwd.display())),
        "the hint names the workspace: {hint}"
    );
    Ok(())
}

/// §11: quit is prompt. An idle Ctrl+Q must not wait out a teardown join —
/// teardown joins stretched exits from 218 ms to 2.9 s and 9.4 s, so the
/// exit is timed here, not only asserted to happen.
#[test]
fn real_pty_idle_ctrl_q_exits_promptly() -> Result<(), Box<dyn Error>> {
    let root = PlakPaths::isolated_tempdir();
    let (mut child, _cwd, _paths, _evidence) = spawn_ready(root.path(), None)?;

    // Let attach settle so the measurement is of an idle quit, not of a
    // startup still in flight.
    child.wait_screen("/ commands")?;
    let started = Instant::now();
    child.send(b"\x11")?;
    let raw = child.wait_exit()?;
    let elapsed = started.elapsed();

    let budget = latency_budget(Duration::from_millis(500));
    assert!(
        elapsed <= budget,
        "idle Ctrl+Q took {elapsed:?}, over the {budget:?} budget"
    );
    assert_terminal_handed_back_cleanly(&raw);
    Ok(())
}

/// §2.1 Ctrl+X with no selection, §6 Ctrl+T, and §5.2 Ctrl+O, as literal bytes.
#[test]
fn real_pty_ctrl_x_ctrl_t_and_ctrl_o_reach_the_control_center_tree_and_last_tool()
-> Result<(), Box<dyn Error>> {
    let root = PlakPaths::isolated_tempdir();
    let (mut child, _cwd, _paths, _evidence) = spawn_ready(root.path(), None)?;

    // Ctrl+X on an empty composer is the control-center chord (it replaced
    // Ctrl+.). It first lands on Session, and the action it dispatches must
    // not re-route the tab onto Settings.
    child.send(b"\x18")?;
    let opened = child.wait_screen("click a value to copy it")?;
    assert!(
        opened.contains("Keybinds") && opened.contains("Build"),
        "the Menu must open on Session and expose its Keybinds tab:\n{opened}"
    );
    // Keybinds is the next tab to the right.
    child.send(b"\x1b[C")?;
    let keybinds = child.wait_screen("quit the TUI")?;
    assert!(
        keybinds.contains("Ctrl+X"),
        "the Keybinds sheet must state the Ctrl+X disambiguation (§2.1):\n{keybinds}"
    );
    assert!(
        !keybinds.contains("Ctrl+."),
        "Ctrl+. is removed with no alias (§2.1):\n{keybinds}"
    );
    child.send(b"\x1b")?;
    child.wait_stable_where("Menu closed", |screen| !screen.contains("Menu"))?;

    // Ctrl+T requests the agent-tree navigator. The offline injected
    // composition attaches no topology hub, so Activity reports that in its body.
    child.send(b"\x14")?;
    let after_tree = child.wait_stable_where("Ctrl+T reached the topology hub", |screen| {
        screen.contains("Activity") && screen.contains(AGENTS_UNAVAILABLE)
    })?;
    assert!(after_tree.contains("no topology hub"), "{after_tree}");
    child.send(b"\x1b")?;
    child.wait_stable_where("Activity closed", |screen| !screen.contains("Activity"))?;

    // A one-call turn is its item line; Alt+O opens it under its summary,
    // never to raw output.
    child.send(b"run the tool so ctrl o has a row to expand\r")?;
    child.wait_screen("loopback turn committed")?;
    let collapsed = child.tool_row("Loopback effect")?;
    assert!(
        collapsed.contains('✓') || collapsed.contains('◌'),
        "the completed Direct tool row must carry a state glyph: `{collapsed}`"
    );
    assert!(
        collapsed.trim_end().ends_with('▸'),
        "an item row ends with its caret: `{collapsed}`"
    );
    child.send(b"\x1bo")?;
    child.wait_stable_where("opened tool group", |screen| {
        screen.contains('▾') && screen.matches("Loopback effect").count() >= 2
    })?;
    assert!(
        !child.screen().contains("loopback effect committed"),
        "opening a group must never reveal raw tool output:\n{}",
        child.screen()
    );

    child.quit_and_wait()?;
    Ok(())
}

/// §4 "Paste routing": a bracketed paste traverses the modal chain, drops
/// `Cc`/`Cf` scalars, and keeps its line structure.
#[test]
fn real_pty_bracketed_paste_drops_control_chars_and_never_reaches_an_open_modal()
-> Result<(), Box<dyn Error>> {
    // U+202E RIGHT-TO-LEFT OVERRIDE is `Cf`: it must not survive into a prompt.
    // The `\r` is the newline a real terminal sends inside a paste envelope.
    const PASTE: &str = "paste alpha line\rpaste omega\u{202e}tail";
    const FIRST: &str = "paste alpha line";
    const SECOND: &str = "paste omegatail";

    let root = PlakPaths::isolated_tempdir();
    let (mut child, cwd, paths, _evidence) = spawn_ready(root.path(), None)?;
    let envelope = format!("\x1b[200~{PASTE}\x1b[201~");

    // A modal owns input. The Keybinds tab has no text sink at all, so the
    // paste must be dropped outright rather than buffered into the composer
    // behind the modal.
    child.send(b"\x18")?;
    child.wait_screen("click a value to copy it")?;
    child.send(b"\x1b[C")?;
    child.wait_screen("quit the TUI")?;
    child.send(envelope.as_bytes())?;
    let over_modal = child.wait_stable_where("keybinds still owns input", |screen| {
        screen.contains("quit the TUI")
    })?;
    assert!(
        !over_modal.contains(FIRST) && !over_modal.contains("paste omega"),
        "paste must never reach the composer while a modal owns input:\n{over_modal}"
    );
    child.send(b"\x1b")?;
    let closed = child.wait_stable_where("modal closed with an untouched composer", |screen| {
        !screen.contains("Menu")
    })?;
    assert!(
        !closed.contains(FIRST) && !closed.contains("paste omega"),
        "the dropped paste must not appear once the modal closes:\n{closed}"
    );

    // Composer owns input: both lines land and the `Cf` scalar is gone.
    child.send(envelope.as_bytes())?;
    let pasted = child.wait_stable_where("multi-line paste in the composer", |screen| {
        screen.contains(FIRST) && screen.contains(SECOND)
    })?;
    assert!(
        pasted.contains(FIRST) && pasted.contains(SECOND),
        "{pasted}"
    );

    // The persisted prompt is the authority: a terminal may itself swallow a
    // zero-width scalar, the product transcript cannot.
    child.send(b"\r")?;
    child.wait_screen("loopback turn committed")?;
    let session = product_sessions::list_sessions_for_cwd(&paths, &cwd)?
        .first()
        .map(|projection| product_sessions::load_session(&paths, &projection.id))
        .transpose()?
        .ok_or("the paste journey left no product session")?;
    let prompt = session
        .transcript
        .iter()
        .find_map(|line| line.text().filter(|text| text.contains("paste omega")))
        .ok_or_else(|| format!("pasted prompt was not committed: {:?}", session.transcript))?;
    assert!(
        !prompt.contains('\u{202e}'),
        "a pasted bidi control scalar reached the committed prompt: {prompt:?}"
    );
    assert!(
        prompt.contains(FIRST) && prompt.contains(SECOND),
        "the pasted lines must survive sanitization: {prompt:?}"
    );

    child.quit_and_wait()?;
    Ok(())
}

/// §4 "Composer editing": Ctrl-arrow word motion plus the Ctrl+W / Ctrl+K /
/// Ctrl+U kills, driven as the bytes a real terminal emits.
#[test]
fn real_pty_ctrl_arrow_word_motion_and_kill_chords_edit_the_draft() -> Result<(), Box<dyn Error>> {
    let root = PlakPaths::isolated_tempdir();
    let (mut child, _cwd, _paths, _evidence) = spawn_ready(root.path(), None)?;

    child.type_draft("alphaone betatwo gammathree")?;

    // Ctrl+W deletes the word before the caret.
    child.send(b"\x17")?;
    let killed_word = child.wait_stable_where("Ctrl+W killed the last word", |screen| {
        !screen.contains("gammathree")
    })?;
    assert!(
        killed_word.contains("alphaone betatwo"),
        "Ctrl+W must kill exactly one word:\n{killed_word}"
    );

    // Ctrl+Left moves one word left; Ctrl+K then kills to line end.
    child.send(b"\x1b[1;5D")?;
    child.send(b"\x0b")?;
    let killed_tail = child
        .wait_stable_where("Ctrl+Left then Ctrl+K killed the tail", |screen| {
            !screen.contains("betatwo")
        })?;
    assert!(
        killed_tail.contains("alphaone"),
        "Ctrl+K must kill only from the caret to line end:\n{killed_tail}"
    );

    // Ctrl+Right moves back over a word so a second Ctrl+K trims only the tail.
    child.send(b"zuluone tangotwo")?;
    child.wait_stable_where("re-typed tail", |screen| {
        screen.contains("alphaone zuluone tangotwo")
    })?;
    child.send(b"\x1b[1;5D")?;
    child.send(b"\x1b[1;5D")?;
    child.send(b"\x1b[1;5C")?;
    child.send(b"\x0b")?;
    let trimmed = child.wait_stable_where("Ctrl+Right bounded the second kill", |screen| {
        !screen.contains("tangotwo")
    })?;
    assert!(
        trimmed.contains("alphaone zuluone"),
        "Ctrl+Right must land on the end of the word it crossed:\n{trimmed}"
    );

    // Ctrl+U kills to line start, leaving an empty composer.
    child.send(b"\x15")?;
    child.wait_draft_absent("alphaone")?;
    let empty = child.screen();
    assert!(
        empty.contains("/ commands") && !empty.contains("zuluone"),
        "Ctrl+U must empty the draft without disturbing the shell:\n{empty}"
    );

    child.quit_and_wait()?;
    Ok(())
}

/// A home with a user profile `pb` whose root binds `binding-b-model` on the
/// codex account at `root_effort`, with one worker card, and topology on.
fn profile_home(
    root_effort: &str,
) -> Result<(tempfile::TempDir, PlakPaths, PathBuf), Box<dyn Error + Send + Sync>> {
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    let paths = PlakPaths::explicit(home.clone());
    paths.ensure_private_state_home()?;
    std::fs::create_dir_all(home.join("agents"))?;
    std::fs::create_dir_all(home.join("profiles"))?;
    std::fs::create_dir_all(&workspace)?;
    std::fs::write(
        home.join("agents/pb-root.toml"),
        format!(
            "account = \"codex\"\nmodel = \"binding-b-model\"\neffort = \"{root_effort}\"\ndescription = \"root\"\n"
        ),
    )?;
    std::fs::write(
        home.join("agents/pb-worker.toml"),
        "account = \"codex\"\nmodel = \"binding-b-model\"\neffort = \"low\"\ndescription = \"Profile B worker card.\"\n",
    )?;
    std::fs::write(
        home.join("profiles/pb.toml"),
        "root = \"pb-root\"\nworking_set = [\"pb-worker\"]\n",
    )?;
    let mut config = crate::product_config::ProductConfig::load(&paths)?;
    config.set_key("orchestration.subagents", "true")?;
    config.save(&paths)?;
    Ok((root, paths, workspace))
}

fn profile_ready_catalog() -> plakous_host_protocol::views::ReadyCatalog {
    vec![plakous_host_protocol::views::ProviderSection {
        account: "codex".to_owned(),
        label: "codex".to_owned(),
        ready: true,
        entries: ["offline-loopback-v1", "binding-b-model"]
            .into_iter()
            .map(|model| plakous_host_protocol::views::ModelEntry {
                id: model.to_owned(),
                tag: "offline exact binding".to_owned(),
                reasoning: true,
                effort_options: vec!["low".to_owned(), "medium".to_owned()],
                extras: Vec::new(),
                unavailable_reason: None,
            })
            .collect(),
    }]
    .into()
}

fn tui_profile_ready_catalog() -> plakous_host_protocol::views::ReadyCatalog {
    [("grok", "grok-4.6"), ("claude", "claude-opus-test")]
        .into_iter()
        .map(
            |(account, model)| plakous_host_protocol::views::ProviderSection {
                account: account.to_owned(),
                label: account.to_owned(),
                ready: true,
                entries: vec![plakous_host_protocol::views::ModelEntry {
                    id: model.to_owned(),
                    tag: "synthetic offline binding".to_owned(),
                    reasoning: true,
                    effort_options: vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
                    extras: Vec::new(),
                    unavailable_reason: None,
                }],
            },
        )
        .collect::<Vec<_>>()
        .into()
}

/// The real TUI's profile dialog must carry a cross-account Fork now all the
/// way through the host before the newly bound session accepts its first turn.
/// The loopback runtimes exercise product integration, not OAuth wire decoding.
#[test]
fn real_tui_profile_fork_now_accepts_first_prompt_and_reopens_target() -> Result<(), Box<dyn Error>>
{
    const SOURCE_PROMPT: &str = "source grok turn before profile fork";
    const TARGET_PROMPT: &str = "first claude turn after profile fork";

    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    let evidence = root.path().join("tui-profile-fork.jsonl");
    let paths = PlakPaths::explicit(home.clone());
    paths.ensure_private_state_home()?;
    std::fs::create_dir_all(home.join("agents"))?;
    std::fs::create_dir_all(home.join("profiles"))?;
    std::fs::create_dir_all(&workspace)?;
    std::fs::write(
        home.join("agents/claude-target.toml"),
        "account = \"claude\"\nmodel = \"claude-opus-test\"\neffort = \"high\"\ndescription = \"Synthetic Claude target.\"\n",
    )?;
    std::fs::write(
        home.join("agents/claude-worker.toml"),
        "account = \"claude\"\nmodel = \"claude-opus-test\"\neffort = \"low\"\ndescription = \"Synthetic worker.\"\n",
    )?;
    std::fs::write(
        home.join("profiles/claude-test.toml"),
        "root = \"claude-target\"\nworking_set = [\"claude-worker\"]\n",
    )?;
    let mut child = PtyChild::spawn_profile_fork(&home, &workspace, &evidence, None)?;
    child.wait_stable_where("source binding context", |screen| screen.contains("%/"))?;
    child.send(format!("{SOURCE_PROMPT}\r").as_bytes())?;
    child.wait_screen("loopback turn committed")?;
    child.send(b"/agents claude-test\r")?;
    child.wait_screen("Fork now")?;
    // Focus starts on Fork now, so Enter forks.
    child.send(b"\r")?;
    child.wait_screen("forked this session onto profile claude-test")?;
    child.wait_screen("claude-test")?;
    child.send(format!("{TARGET_PROMPT}\r").as_bytes())?;
    child.wait_stable_where(
        "successful terminal response after profile fork",
        |screen| {
            screen.contains(TARGET_PROMPT)
                && screen.matches("loopback turn committed").count() >= 2
                && screen.contains("/ commands")
                && !screen.contains("thinking")
                && !screen.contains("responding")
        },
    )?;
    child.quit_and_wait()?;

    let sessions = product_sessions::list_sessions_for_cwd(&paths, &workspace)?;
    assert_eq!(
        sessions.len(),
        2,
        "profile fork must preserve two identities"
    );
    let records = sessions
        .iter()
        .map(|row| product_sessions::load_session(&paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    let source = records
        .iter()
        .find(|session| session.model.as_deref() == Some("grok-4.6"))
        .ok_or("durable source session missing")?;
    let target = records
        .iter()
        .find(|session| session.model.as_deref() == Some("claude-opus-test"))
        .ok_or("durable target session missing")?;
    assert_ne!(source.id, target.id);
    assert_eq!(source.account.as_deref(), Some("grok"));
    assert_eq!(target.account.as_deref(), Some("claude"));
    assert!(
        source
            .transcript
            .iter()
            .any(|line| line.text().is_some_and(|text| text.contains(SOURCE_PROMPT)))
    );
    assert!(
        !source
            .transcript
            .iter()
            .any(|line| line.text().is_some_and(|text| text.contains(TARGET_PROMPT)))
    );
    assert!(
        target
            .transcript
            .iter()
            .any(|line| line.text().is_some_and(|text| text.contains(TARGET_PROMPT)))
    );
    assert!(
        target
            .transcript
            .iter()
            .filter(|line| line
                .text()
                .is_some_and(|text| text.contains("loopback turn committed")))
            .count()
            >= 2,
        "target must commit a new response, not merely inherit the source response"
    );

    let mut reopened =
        PtyChild::spawn_profile_fork(&home, &workspace, &evidence, Some(&target.id))?;
    reopened.wait_stable_where("resumed target binding context", |screen| {
        screen.contains("%/")
    })?;
    reopened.wait_screen(TARGET_PROMPT)?;
    reopened.quit_and_wait()?;
    Ok(())
}

fn profile_outcome(
    event: &crate::surface_protocol::HostEvent,
) -> Option<Result<crate::surface_protocol::ProfileOutcome, String>> {
    use crate::surface_protocol::SurfaceObservation;
    match event {
        crate::surface_protocol::HostEvent::Extension(SurfaceObservation::ProfileApplied {
            outcome,
            ..
        }) => Some(Ok(*outcome)),
        crate::surface_protocol::HostEvent::Extension(SurfaceObservation::ProfileRefused {
            reason,
            ..
        }) => Some(Err(reason.clone())),
        _ => None,
    }
}

/// Fork now on an attached session branches onto the profile with a tree of
/// its own, saves the profile only after the branch lands, and leaves the
/// source on its own tree so it still reopens under the strict tool check.
#[test]
fn profile_fork_now_branches_onto_its_own_tree_and_saves_after_it_lands()
-> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"profile-source-account";
    const B: &[u8] = b"profile-target-account";
    let (root, paths, workspace) = profile_home("low")?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("profile-fork.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut source)?;
    let source_id = source.id.clone();

    let mut config = host_config(&paths, &workspace, source);
    config.ready_catalog = profile_ready_catalog();
    // The source attachment join is held half a second past runtime release, so
    // the quit below always lands while it is pending.
    config.runtime_factory = account_injection(&workspace, &controller, A, B)?
        .with_source_release_delay(Duration::from_millis(500));
    let (actions, events, worker) = crate::product_host::spawn_host_worker(config)?;
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("source turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    let config_before = std::fs::read_to_string(paths.config_file())?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: "pb".to_owned(),
                fork_now: true,
            },
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    let outcome = observed.iter().rev().find_map(profile_outcome);
    assert_eq!(
        outcome,
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    let quit_at = Instant::now();
    worker
        .join()
        .map_err(|_| "profile host panicked")?
        .map_err(|error| error.message().to_owned())?;
    // The host ended only after the source attachment join finished, inside the
    // launcher's worker join deadline.
    assert!(quit_at.elapsed() < crate::commands::support::HOST_WORKER_JOIN_TIMEOUT);
    let source = product_sessions::load_session(&paths, &source_id)?;
    let store: Arc<dyn plakous_hosted::session_store::SessionRecordStore> = Arc::new(
        crate::store::session_records::SqliteSessionStore::new(paths.clone()),
    );
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let projected = ApplicationExecutor::new()?
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &store, &source,
        ))?
        .map_err(|error| format!("source projection failed: {error}"))?
        .ok_or("source projection missing")?;
    let source_views = store
        .load_views(&source_id)?
        .ok_or("source views missing")?;
    assert_eq!(
        source_views.state.head.map(|head| head.checkpoint_id),
        Some(projected.head.checkpoint_id())
    );

    let saved = crate::product_config::ProductConfig::load(&paths)?;
    assert_ne!(std::fs::read_to_string(paths.config_file())?, config_before);
    assert_eq!(saved.root_agent.as_deref(), Some("pb-root"));
    let target = product_sessions::list_sessions_for_cwd(&paths, &workspace)?
        .into_iter()
        .find(|row| row.id != source_id)
        .ok_or("the profile fork published no new session")?;
    let target = product_sessions::load_session(&paths, &target.id)?;
    assert_eq!(target.model.as_deref(), Some("binding-b-model"));
    // The new session's tree is its own and froze the profile's card.
    let target_tree = plakous_hosted::runtime::TreeRows::tree(&paths, &target.id)?
        .ok_or("the profile fork opened no tree of its own")?;
    assert!(
        target_tree
            .snapshot
            .roster
            .keys()
            .any(|name| name.as_str() == "pb-worker")
    );
    // The source's tree, if it has one, never received the profile's cards.
    if let Some(source_tree) = plakous_hosted::runtime::TreeRows::tree(&paths, &source_id)? {
        assert!(
            !source_tree
                .snapshot
                .roster
                .keys()
                .any(|name| name.as_str() == "pb-worker")
        );
    }

    // The source reopens and runs a turn on its own binding.
    let mut reopen = host_config(
        &paths,
        &workspace,
        product_sessions::load_session(&paths, &source_id)?,
    );
    reopen.ready_catalog = profile_ready_catalog();
    reopen.runtime_factory = account_injection(&workspace, &controller, A, B)?;
    let (actions, events, worker) = crate::product_host::spawn_host_worker(reopen)?;
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("source again".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "reopened source host panicked")?
        .map_err(|error| error.message().to_owned())?;
    Ok(())
}

/// A Fork now the host refuses saves nothing: the default for the next
/// launch is unchanged and no new session exists.
#[test]
fn a_refused_profile_fork_saves_nothing() -> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"profile-refused-source";
    const B: &[u8] = b"profile-refused-target";
    // `high` is absent from the catalog row, so the rebind refuses the root.
    let (root, paths, workspace) = profile_home("high")?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("profile-refused.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut source)?;
    let config_before = std::fs::read_to_string(paths.config_file())?;

    let mut config = host_config(&paths, &workspace, source);
    config.ready_catalog = profile_ready_catalog();
    config.runtime_factory = account_injection(&workspace, &controller, A, B)?;
    let (actions, events, worker) = crate::product_host::spawn_host_worker(config)?;
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: "pb".to_owned(),
                fork_now: true,
            },
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert!(
        matches!(
            observed.iter().rev().find_map(profile_outcome),
            Some(Err(_))
        ),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "refused profile host panicked")?
        .map_err(|error| error.message().to_owned())?;
    assert_eq!(std::fs::read_to_string(paths.config_file())?, config_before);
    assert_eq!(
        product_sessions::list_sessions_for_cwd(&paths, &workspace)?.len(),
        1
    );
    Ok(())
}

/// Fork now onto the profile this session already runs, root and cards, is
/// not a fork: nothing branches and the outcome says so.
#[test]
fn fork_now_onto_the_running_profile_is_already_active() -> Result<(), Box<dyn Error + Send + Sync>>
{
    same_profile_fork(true)
}

#[test]
fn fork_now_onto_a_treeless_running_profile_seeds_its_own_tree()
-> Result<(), Box<dyn Error + Send + Sync>> {
    same_profile_fork(false)
}

fn same_profile_fork(with_tree: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"profile-same-source";
    const B: &[u8] = b"profile-new-tree";
    let (root, paths, workspace) = profile_home("low")?;
    let mut config = crate::product_config::ProductConfig::load(&paths)?;
    config.set_key("root.agent", "pb-root")?;
    config.set_key("roster.working_set", "pb-worker")?;
    config.save(&paths)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("profile-same.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("binding-b-model".to_owned());
    source.effort = Some("low".to_owned());
    product_sessions::save_session(&paths, &mut source)?;

    let source_id = source.id.clone();
    if with_tree {
        let roster = crate::roster_input::roster_input_from_config(
            &config,
            "codex",
            "binding-b-model",
            Some("low".to_owned()),
        );
        let mut tree = plakous_hosted::agent_tree::ProductAgentTree::bootstrap_root(
            &source.id,
            &source.id,
            "root",
            workspace.clone(),
            "binding-b-model",
            roster.writer_mode,
        )
        .map_err(|error| error.message)?;
        tree.install_snapshot(plakous_hosted::orchestration::OrchestrationSnapshot::freeze(roster));
        let _revision = crate::store::tree::tree_store(&paths)?
            .save(&tree)
            .map_err(|error| error.detail)?;
    }

    let mut host = host_config(&paths, &workspace, source);
    host.ready_catalog = profile_ready_catalog();
    host.runtime_factory =
        account_injection(&workspace, &controller, A, if with_tree { A } else { B })?;
    let (actions, events, worker) = crate::product_host::spawn_host_worker(host)?;
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("profile turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: "pb".to_owned(),
                fork_now: true,
            },
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().rev().find_map(profile_outcome),
        Some(Ok(if with_tree {
            crate::surface_protocol::ProfileOutcome::AlreadyActive
        } else {
            crate::surface_protocol::ProfileOutcome::Forked
        })),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "same-profile host panicked")?
        .map_err(|error| error.message().to_owned())?;
    assert_eq!(
        product_sessions::list_sessions_for_cwd(&paths, &workspace)?.len(),
        if with_tree { 1 } else { 2 }
    );
    if !with_tree {
        let target = product_sessions::list_sessions_for_cwd(&paths, &workspace)?
            .into_iter()
            .find(|row| row.id != source_id)
            .ok_or("profile fork missing")?;
        assert!(plakous_hosted::runtime::TreeRows::tree(&paths, &target.id)?.is_some());
        assert!(plakous_hosted::runtime::TreeRows::tree(&paths, &source_id)?.is_none());
    }
    Ok(())
}

/// A session resumed with a pause left over from earlier work (and
/// nothing queued) drains a follow-up typed during a live turn; it does not
/// sit "paused with the tree" until the operator resumes it by hand.
#[test]
fn a_live_follow_up_drains_after_a_resume_with_a_stale_pause()
-> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"stale-pause-account";
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&workspace)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("stale-pause.jsonl"),
    )?;
    let paths = PlakPaths::explicit(home);
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("binding-a".to_owned());
    source.model = Some("binding-a-model".to_owned());
    source.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut source)?;
    crate::store::ProductStore::open(&paths)?.write_host_controls(
        &source.id,
        false,
        Some(
            plakous_hosted::sessions::projector::HostPauseCause::UnrecordedRefusal {
                reason: "earlier refusal".to_owned(),
            },
        ),
    )?;
    let id = source.id.clone();
    let source = product_sessions::load_session(&paths, &id)?;
    assert_eq!(
        source.queue_drain_state,
        plakous_hosted::sessions::QueueDrainState::RestoredPaused
    );

    let mut config = host_config(&paths, &workspace, source);
    config.ready_catalog = dual_binding_ready_catalog();
    config.runtime_factory = account_injection(&workspace, &controller, A, A)?;
    let (actions, events, worker) = crate::product_host::spawn_host_worker(config)?;
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("first".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::SetBusy(true))
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::FollowUp("typed while busy".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "stale-pause host panicked")?
        .map_err(|error| error.message().to_owned())?;
    let after = product_sessions::load_session(&paths, &id)?;
    assert_eq!(
        after.pending_queue,
        [] as [plakous_hosted::sessions::QueueItem; 0]
    );
    assert_eq!(
        after.prompt_history.last().map(String::as_str),
        Some("typed while busy")
    );
    Ok(())
}
