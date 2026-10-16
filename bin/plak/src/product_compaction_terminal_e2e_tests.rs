//! Real-PTY qualification of the exact Plak compaction journey.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "private process-level test harness"
)]

use std::error::Error;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plakous::agent::{ReasoningEffort, TurnResultOutcome};
use plakous::application::ApplicationExecutor;
use plakous::model::{ContentBlock, ContextStatusKind};
use plakous::sessions::{
    AgentSessions, ContextQuery, ContextStatusRequest, CreateSessionOutcome, CreateSessionRequest,
    OpenSessionOutcome, OpenSessionRequest, SessionBackend, SessionContextStatusKind,
    SessionOperationId,
};
use plakous::subscriptions::NativeProvider;
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{
    BoundaryController, compaction_runtime_binding_with_projected_input,
    compaction_runtime_binding_with_projected_input_and_identity_namespace,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::product_host::{HostConfig, TestHostInjection, TestHostWorkflow};
use crate::product_paths::PlakPaths;
use crate::product_sessions::{self, SessionRecord};
use crate::{HostedAgentChannels, ProcessOutcome, TuiAttacher, launch_tui_with_attacher};

const CHILD_TEST: &str = "product_compaction_terminal_e2e_tests::compaction_terminal_e2e_child";
const WAIT: Duration = Duration::from_secs(45);
const ROWS: u16 = 30;
const COLS: u16 = 100;
const PROMPTS: [&str; 3] = [
    "compaction intent group alpha with deliberately substantial replaceable context AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "compaction intent group beta",
    "compaction retained causal tail gamma",
];
const FOLLOWING_PROMPT: &str = "qz";
const SUMMARY: &str = "older intent groups compacted";

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

struct CompactionAttacher {
    evidence: PathBuf,
    boundaries: PathBuf,
    maximum_projected_input: u64,
}

impl TuiAttacher for CompactionAttacher {
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
        resume_publication: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<HostedAgentChannels, String> {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &self.evidence)
                .map_err(|error| error.to_string())?;
        let target = compaction_runtime_binding_with_projected_input(
            cwd,
            controller.clone(),
            self.maximum_projected_input,
        )
        .map_err(|error| error.to_string())?
        .target_binding();
        let workspace = cwd.to_path_buf();
        let maximum_projected_input = self.maximum_projected_input;
        let process_namespace = u128::from(std::process::id()) << 32;
        let identity_sequence = Arc::new(AtomicU64::new(1));
        let runtime = Arc::new(move |_| {
            let identity_namespace =
                process_namespace | u128::from(identity_sequence.fetch_add(1, Ordering::Relaxed));
            compaction_runtime_binding_with_projected_input_and_identity_namespace(
                &workspace,
                controller.clone(),
                maximum_projected_input,
                identity_namespace,
            )
            .map_err(|error| error.to_string())
        });
        let observed_paths = paths.clone();
        let observed_session = session.id.clone();
        let boundaries = self.boundaries.clone();
        let observe = Arc::new(move |workflow, boundary| {
            if workflow != TestHostWorkflow::Compact {
                return;
            }
            let locator = product_sessions::load_session(&observed_paths, &observed_session)
                .ok()
                .and_then(|record| record.plakous_locator)
                .map(|locator| locator.to_string())
                .unwrap_or_else(|| "missing".to_owned());
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&boundaries)
                .expect("open compact boundary evidence");
            writeln!(file, "{boundary:?}\t{locator}").expect("write compact boundary evidence");
            file.sync_data().expect("sync compact boundary evidence");
        });
        let injection = Arc::new(TestHostInjection {
            runtime,
            rebind_target: target,
            observe,
            child_runtime: None,
            rebind_builds: Mutex::new(Vec::new()),
        });
        crate::product_host::spawn_host_worker(HostConfig {
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
            target: crate::product_host::HostTargetAuthority::Injected {
                provider: NativeProvider::Codex,
            },
            model_id: "offline-loopback-v1".to_owned(),
            effort: Some(effort),
            effort_explicit,
            compaction_enabled: true,
            max_model_rounds: crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_enabled: crate::product_config::SessionPolicy::load(paths)
                .unwrap()
                .topology_enabled,
            root_template: None,
            attached_one_shot: false,
            interactive: true,
            cwd: cwd.to_path_buf(),
            ready_catalog: ready_catalog.clone(),
            resume_publication,
            test_injection: Some(injection),
            ready_path: crate::product_host::ReadyPathTiming::default(),
            seed_history: None,
            source_releases: crate::product_host::SourceReleases::default(),
        })
        .map_err(|failure| failure.message().to_owned())
    }
}

#[test]
#[ignore = "nested real-terminal fixture"]
fn compaction_terminal_e2e_child() -> TestResult {
    let Some(home) = std::env::var_os("PLAK_COMPACT_PTY_HOME") else {
        return Ok(());
    };
    let paths = PlakPaths::explicit(PathBuf::from(home));
    let mut config = crate::product_config::ProductConfig::default_product();
    config.bind_root_tuple("codex", "gpt-offline-loopback", Some("medium"));
    config.compaction_enabled = true;
    config.save(&paths)?;
    let attacher = CompactionAttacher {
        evidence: PathBuf::from(
            std::env::var_os("PLAK_COMPACT_PTY_EVIDENCE").ok_or("evidence path missing")?,
        ),
        boundaries: PathBuf::from(
            std::env::var_os("PLAK_COMPACT_PTY_BOUNDARIES")
                .ok_or("boundary evidence path missing")?,
        ),
        maximum_projected_input: std::env::var("PLAK_COMPACT_PTY_MAXIMUM")?.parse()?,
    };
    let resume = std::env::var("PLAK_COMPACT_PTY_RESUME").ok();
    let services = crate::shell::services::Services::from_env(None);
    let outcome = launch_tui_with_attacher(&services, resume, false, None, &attacher)
        .map_err(|failure| failure.message().to_owned())?;
    if outcome != ProcessOutcome::Success {
        return Err("compaction PTY child did not exit successfully".into());
    }
    Ok(())
}

struct PtyChild {
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    terminal: Arc<Mutex<TerminalCapture>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

struct TerminalCapture {
    parser: vt100::Parser,
    raw: Vec<u8>,
}

impl PtyChild {
    fn spawn(
        home: &Path,
        cwd: &Path,
        evidence: &Path,
        boundaries: &Path,
        maximum: u64,
        resume: Option<&str>,
    ) -> TestResult<Self> {
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
        command.env("PLAK_COMPACT_PTY_HOME", home);
        command.env("XDG_CONFIG_HOME", home.join("xdg-config"));
        command.env("XDG_DATA_HOME", home.join("xdg-data"));
        command.env("XDG_STATE_HOME", home.join("xdg-state"));
        command.env("PLAK_COMPACT_PTY_EVIDENCE", evidence);
        command.env("PLAK_COMPACT_PTY_BOUNDARIES", boundaries);
        command.env("PLAK_COMPACT_PTY_MAXIMUM", maximum.to_string());
        command.env("TERM", "xterm-256color");
        command.env("LANG", "C.UTF-8");
        command.env("NO_COLOR", "1");
        if let Some(resume) = resume {
            command.env("PLAK_COMPACT_PTY_RESUME", resume);
        }
        command.cwd(cwd);
        let mut source = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let terminal = Arc::new(Mutex::new(TerminalCapture {
            parser: vt100::Parser::new(ROWS, COLS, 0),
            raw: Vec::new(),
        }));
        let sink = Arc::clone(&terminal);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            while let Ok(count) = source.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let mut capture = sink.lock().unwrap();
                capture.parser.process(&buffer[..count]);
                capture.raw.extend_from_slice(&buffer[..count]);
            }
        });
        Ok(Self {
            writer,
            child,
            terminal,
            reader: Some(reader),
        })
    }

    fn screen(&self) -> String {
        self.terminal.lock().unwrap().parser.screen().contents()
    }

    fn send(&mut self, text: &str) -> TestResult {
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    fn wait_screen(&mut self, needle: &str) -> TestResult<String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let first = self.screen();
            if first.contains(needle) {
                std::thread::sleep(Duration::from_millis(30));
                let second = self.screen();
                if first == second && second.contains(needle) {
                    return Ok(second);
                }
            }
            if let Some(status) = self.child.try_wait()? {
                return Err(format!("PTY exited before `{needle}`: {status:?}\n{first}").into());
            }
            if Instant::now() >= deadline {
                return Err(format!("timed out waiting for `{needle}`\n{first}").into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn quit(mut self) -> TestResult<Vec<u8>> {
        self.send("/quit\r")?;
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                if !status.success() {
                    return Err(format!("PTY child failed: {status:?}").into());
                }
                break;
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                return Err("PTY child required forced cleanup".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| "PTY reader panicked")?;
        }
        Ok(self.terminal.lock().unwrap().raw.clone())
    }
}

fn exact_measurement(status: &plakous::model::ContextStatus) -> TestResult<u64> {
    Ok(status
        .measurement()
        .ok_or("context status omitted its exact prepared measurement")?
        .bound()
        .upper_bound())
}

fn calibrate_ceiling(workspace: &Path, evidence: &Path, database: &Path) -> TestResult<u64> {
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, evidence)?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(database)?)?;
    let executor = ApplicationExecutor::new()?;
    let CreateSessionOutcome::Created { session, .. } =
        executor.block_on(sessions.create(CreateSessionRequest::new(
            SessionOperationId::new("compact-pty-calibration:create")?,
            compaction_runtime_binding_with_projected_input(
                workspace,
                controller.clone(),
                10_000_000,
            )?,
        )))??
    else {
        return Err("calibration session creation returned an unknown outcome".into());
    };
    for prompt in &PROMPTS[..2] {
        let result = executor
            .block_on(async { session.agent().prompt(*prompt).await?.result().await })??;
        if !matches!(result.outcome(), TurnResultOutcome::Completed(_)) {
            return Err(
                format!("calibration turn did not complete: {:?}", result.outcome()).into(),
            );
        }
    }
    let current = executor.block_on(sessions.context_status(ContextStatusRequest::new(
        SessionOperationId::new("compact-pty-calibration:current-two")?,
        session.locator(),
        ContextQuery::Current,
    )))??;
    let SessionContextStatusKind::Available(current) = current.kind() else {
        return Err("calibration current projection was unavailable".into());
    };
    let current_measurement = exact_measurement(current)?;
    let pending = executor.block_on(sessions.context_status(ContextStatusRequest::new(
        SessionOperationId::new("compact-pty-calibration:pending-third")?,
        session.locator(),
        ContextQuery::WithPendingUserInput(PROMPTS[2].to_owned().into()),
    )))??;
    let SessionContextStatusKind::Available(pending) = pending.kind() else {
        return Err("calibration pending projection was unavailable".into());
    };
    let pending_measurement = exact_measurement(pending)?;
    if pending_measurement <= current_measurement {
        return Err("fixed pending prompt did not add measurable context pressure".into());
    }
    let ceiling = current_measurement + (pending_measurement - current_measurement) / 2;
    let third =
        executor.block_on(async { session.agent().prompt(PROMPTS[2]).await?.result().await })??;
    if !matches!(third.outcome(), TurnResultOutcome::Completed(_)) {
        return Err("calibration third turn did not complete".into());
    }
    let completed = executor.block_on(sessions.context_status(ContextStatusRequest::new(
        SessionOperationId::new("compact-pty-calibration:completed-third")?,
        session.locator(),
        ContextQuery::Current,
    )))??;
    let SessionContextStatusKind::Available(completed) = completed.kind() else {
        return Err("calibration completed projection was unavailable".into());
    };
    if exact_measurement(completed)? <= pending_measurement {
        return Err(
            "fixed completed group did not grow beyond its admitted pending projection".into(),
        );
    }
    let _ = executor.block_on(session.release())??;
    Ok(ceiling)
}

fn provider_dispatch_count(path: &Path) -> TestResult<usize> {
    Ok(read_evidence(path)?
        .iter()
        .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
        .count())
}

fn session_event_count(paths: &PlakPaths, session_id: &str, kind: &str) -> usize {
    std::fs::read_to_string(paths.session_events_file(session_id))
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|event| {
                    event
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .is_some_and(|observed| observed == kind)
        })
        .count()
}

fn wait_session_event_count(
    paths: &PlakPaths,
    session_id: &str,
    kind: &str,
    expected: usize,
) -> TestResult {
    let deadline = Instant::now() + WAIT;
    loop {
        if session_event_count(paths, session_id, kind) >= expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for product event `{kind}` #{expected}").into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_committed_prompt(
    paths: &PlakPaths,
    workspace: &Path,
    evidence: &Path,
    prompt: &str,
    dispatch_count: usize,
) -> TestResult {
    let deadline = Instant::now() + WAIT;
    loop {
        let provider_outcomes = read_evidence(evidence)?
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderOutcome)
            .count();
        let committed = product_sessions::list_sessions_for_cwd(paths, workspace)?
            .iter()
            .filter_map(|projection| product_sessions::load_session(paths, &projection.id).ok())
            .any(|record| {
                record
                    .prompt_history
                    .last()
                    .is_some_and(|value| value == prompt)
                    && record.pending_queue.is_empty()
                    && record
                        .transcript
                        .iter()
                        .filter(|line| line.text().is_some_and(|text| text.contains(SUMMARY)))
                        .count()
                        >= record.prompt_history.len()
                    && record.plakous_locator.is_some()
            });
        if provider_dispatch_count(evidence)? == dispatch_count
            && provider_outcomes == dispatch_count
            && committed
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for exact prompt commit `{prompt}` at dispatch {dispatch_count}"
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "on-demand real-terminal qualification"]
fn real_pty_compacts_exact_overflow_reopens_and_continues() -> TestResult {
    let root = tempfile::tempdir()?;
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&workspace)?;
    let calibration = root.path().join("calibration.jsonl");
    let calibration_database = root.path().join("calibration.sqlite3");
    let ceiling = calibrate_ceiling(&workspace, &calibration, &calibration_database)
        .map_err(|error| format!("compaction PTY calibration failed: {error}"))?;
    let evidence = root.path().join("physical.jsonl");
    let boundaries = root.path().join("compact-boundaries.tsv");
    let paths = PlakPaths::explicit(home.clone());

    let mut child = PtyChild::spawn(&home, &workspace, &evidence, &boundaries, ceiling, None)?;
    child.wait_screen("/ commands")?;
    for (index, prompt) in PROMPTS[..2].iter().copied().enumerate() {
        child.send(&format!("{prompt}\r"))?;
        child.wait_screen(prompt.get(..32).unwrap_or(prompt))?;
        wait_for_committed_prompt(&paths, &workspace, &evidence, prompt, index + 1).map_err(
            |error| {
                format!(
                    "{error}\nterminal:\n{}\nphysical evidence: {:?}",
                    child.screen(),
                    read_evidence(&evidence).unwrap_or_default()
                )
            },
        )?;
        child.wait_screen(SUMMARY)?;
    }
    let projections = product_sessions::list_sessions_for_cwd(&paths, &workspace)?;
    let [projection] = projections.as_slice() else {
        return Err("PTY setup did not publish exactly one product session".into());
    };
    let source_record = product_sessions::load_session(&paths, &projection.id)?;
    let source = source_record
        .plakous_locator
        .ok_or("compact source product record omitted its locator")?;
    let raw = child.quit()?;
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let executor = ApplicationExecutor::new()?;
    let opened_source = executor.block_on(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("compact-pty:exact-source-open")?,
        source,
        compaction_runtime_binding_with_projected_input(&workspace, controller.clone(), ceiling)?,
    )))??;
    let OpenSessionOutcome::Opened {
        session: source_session,
        ..
    } = opened_source
    else {
        return Err(format!("compact source did not reopen quiescently: {opened_source:?}").into());
    };
    let before = executor
        .block_on(sessions.context_status(ContextStatusRequest::new(
            SessionOperationId::new("compact-pty:verify-current-fits")?,
            source,
            ContextQuery::Current,
        )))?
        .map_err(|error| format!("compaction PTY source status failed for {source}: {error}"))?;
    assert!(matches!(
        before.kind(),
        SessionContextStatusKind::Available(status)
            if matches!(status.kind(), ContextStatusKind::Fits { .. })
    ));
    let prospective = executor
        .block_on(sessions.context_status(ContextStatusRequest::new(
            SessionOperationId::new("compact-pty:verify-pending-overflow")?,
            source,
            ContextQuery::WithPendingUserInput(PROMPTS[2].to_owned().into()),
        )))?
        .map_err(|error| format!("compaction PTY pending status failed for {source}: {error}"))?;
    assert!(matches!(
        prospective.kind(),
        SessionContextStatusKind::Available(status)
            if matches!(status.kind(), ContextStatusKind::WouldOverflow(_))
    ));
    assert_eq!(provider_dispatch_count(&evidence)?, 2);
    assert_eq!(executor.block_on(source_session.release())??, source);

    let ready_count = session_event_count(&paths, &projection.id, "session.ready");
    let mut child = PtyChild::spawn(
        &home,
        &workspace,
        &evidence,
        &boundaries,
        ceiling,
        Some(&projection.id),
    )?;
    wait_session_event_count(&paths, &projection.id, "session.ready", ready_count + 1)?;
    child.wait_screen(PROMPTS[1])?;
    child.send(&format!("{}\r", PROMPTS[2]))?;
    child.wait_screen(PROMPTS[2])?;
    wait_for_committed_prompt(&paths, &workspace, &evidence, PROMPTS[2], 4).map_err(|error| {
        format!(
            "{error}\nterminal:\n{}\nphysical evidence: {:?}",
            child.screen(),
            read_evidence(&evidence).unwrap_or_default()
        )
    })?;
    let compacted = child.wait_screen(SUMMARY)?;
    assert!(!compacted.to_ascii_lowercase().contains("recovery required"));
    assert_eq!(provider_dispatch_count(&evidence)?, 4);
    let raw = child.quit()?;
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));

    let target_record = product_sessions::load_session(&paths, &projection.id)?;
    let target = target_record
        .plakous_locator
        .ok_or("compact product publication omitted its target locator")?;
    assert_ne!(source, target);
    let mut store = crate::store::ProductStore::open(&paths)?;
    let intents = store.load_intents(&projection.id)?;
    let request = intents
        .workflow_requests
        .iter()
        .find(|request| {
            matches!(
                request.kind,
                product_sessions::SessionWorkflowKind::Compact(
                    product_sessions::ProductCompactReason::Scheduled
                )
            )
        })
        .ok_or("scheduled compaction request missing")?;
    let views = store
        .load_views(&projection.id)?
        .ok_or("compaction views missing")?;
    assert!(matches!(
        views.state.workflows[&request.operation_id].settled,
        Some(plakous_hosted::sessions::projector::WorkflowSettlement::Compacted { .. })
    ));
    let boundary_rows = std::fs::read_to_string(&boundaries)?;
    let compact_target = boundary_rows
        .lines()
        .find_map(|line| line.strip_prefix("ProductPublished\t"))
        .ok_or("compaction publication boundary missing")?
        .parse::<plakous::sessions::SessionLocator>()?;
    assert_ne!(source, compact_target);
    assert_ne!(compact_target, target);

    assert!(
        boundary_rows.contains(&format!("Prepared\t{source}")),
        "{boundary_rows}"
    );
    assert!(
        boundary_rows.contains(&format!("RuntimeReturned\t{source}")),
        "{boundary_rows}"
    );
    assert!(
        boundary_rows.contains(&format!("ProductPublished\t{compact_target}")),
        "{boundary_rows}"
    );

    let opened = executor.block_on(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("compact-pty:exact-target-open")?,
        target,
        compaction_runtime_binding_with_projected_input(&workspace, controller.clone(), ceiling)?,
    )))??;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err(format!("compact target did not reopen quiescently: {opened:?}").into());
    };
    let history = session.agent().conversation();
    let texts = history
        .items()
        .into_iter()
        .flat_map(plakous::model::ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(texts.contains(&SUMMARY), "compacted history: {texts:?}");
    assert!(texts.contains(&PROMPTS[1]), "compacted history: {texts:?}");
    assert!(texts.contains(&PROMPTS[2]), "compacted history: {texts:?}");
    assert!(!texts.contains(&PROMPTS[0]), "compacted history: {texts:?}");
    assert_eq!(executor.block_on(session.release())??, target);

    let ready_count = session_event_count(&paths, &projection.id, "session.ready");
    let mut resumed = PtyChild::spawn(
        &home,
        &workspace,
        &evidence,
        &boundaries,
        ceiling,
        Some(&projection.id),
    )?;
    wait_session_event_count(&paths, &projection.id, "session.ready", ready_count + 1)?;
    resumed.wait_screen(PROMPTS[2])?;
    resumed.send(&format!("{FOLLOWING_PROMPT}\r"))?;
    let continued = resumed.wait_screen(FOLLOWING_PROMPT)?;
    wait_for_committed_prompt(&paths, &workspace, &evidence, FOLLOWING_PROMPT, 6).map_err(
        |error| {
            format!(
                "{error}\nterminal:\n{}\nphysical evidence: {:?}",
                resumed.screen(),
                read_evidence(&evidence).unwrap_or_default()
            )
        },
    )?;
    resumed.wait_screen(SUMMARY)?;
    assert!(!continued.to_ascii_lowercase().contains("recovery required"));
    assert_eq!(provider_dispatch_count(&evidence)?, 6);
    let raw = resumed.quit()?;
    assert!(raw.windows(7).any(|window| window == b"[?1049l"));
    assert_eq!(
        read_evidence(&evidence)?
            .iter()
            .filter(|row| row.kind == EvidenceKind::EffectDispatched)
            .count(),
        0
    );
    Ok(())
}
