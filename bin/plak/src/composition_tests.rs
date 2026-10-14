//! Composition tests: the hosted loop over Plak's own durable authorities.
//!
//! Every test here reads a product file back -- `product.sqlite3`,
//! `runtime.sqlite3`, `config.toml` -- after driving the hosted host loop. The
//! hosted crate's tests run on port doubles (`plakous_hosted::runtime::test_kit`);
//! these cannot, because what they pin *is* the composition: that the loop's
//! writes reach the authorities Plak keeps, in the order it promises.
//!
//! They lived in the hosted crate's unit tests until the product half moved
//! here. Names are unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeProvider;
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{
    BoundaryController, overflow_runtime_binding, runtime_binding,
};

use crate::home::paths::PlakPaths;
use plakous_hosted::runtime::{
    COMPLETION_WINS_QUIT_PROMPT, HostConfig, HostTargetAuthority, ReadyPathTiming,
    TestHostBoundary, TestHostInjection, TestHostWorkflow, TestRuntimeRole, run_injected_host,
    spawn_host_worker,
};
use plakous_hosted::sessions::TranscriptRow;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous::application::ApplicationExecutor;

use crate::shell::settings::ProductSurface;
use crate::store::sessions::{self, SessionRecord};
use crate::surface_protocol;
use plakous::sessions::SessionOperationId as HostSessionOperationId;
use plakous_hosted::agent::build_or_open_injected_session;
use plakous_hosted::binding::{CompositionSelection as ProductComposition, TargetSelection};
use plakous_hosted::runtime::{Sender, clear_session_rebuild};
use surface_protocol::{HostEvent, SurfaceIntent, UserAction};

use crate::product_config::SessionPolicy;

/// The attached host of a temporary product home, over Plak's own
/// implementations of every port.
pub(crate) fn product_injected_attached_host(
    root: &std::path::Path,
) -> (
    HostConfig<surface_protocol::PlakExtension>,
    Arc<TestHostInjection>,
    PlakPaths,
) {
    let paths = PlakPaths::explicit(root.join("home"));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    paths.ensure_layout().unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.join("immediate-quit-evidence.jsonl"),
    )
    .unwrap();
    let target = runtime_binding(&workspace, controller.clone())
        .unwrap()
        .target_binding();
    let runtime_workspace = workspace.clone();
    let injection = Arc::new(TestHostInjection {
        runtime: Arc::new(move |_| {
            runtime_binding(&runtime_workspace, controller.clone())
                .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let mut session = SessionRecord::new(workspace.clone());
    session.account = Some("codex".into());
    session.model = Some("offline-loopback-v1".into());
    session.effort = Some("medium".into());
    session.composition = plakous_hosted::binding::CompositionSelection::JsPtc;
    let config = HostConfig {
        surface: Some(ProductSurface::boxed(&paths)),
        auto_continue: true,
        topology_admission: Some(crate::product_config::topology_admission_probe(&paths)),
        state_roots: crate::shell::host::product_state_roots(&paths),
        home: paths.session_home(),
        store: Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        )),
        topology_enabled: SessionPolicy::load(&paths).unwrap().topology_enabled,
        product_home: paths.home().to_path_buf(),
        session,
        target: HostTargetAuthority::Injected {
            provider: NativeProvider::Codex,
        },
        model_id: "offline-loopback-v1".into(),
        effort: Some(ReasoningEffort::Medium),
        effort_explicit: true,
        compaction_enabled: false,
        max_model_rounds: crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        root_template: None,
        attached_one_shot: false,
        interactive: true,
        cwd: workspace,
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        resume_publication: None,
        test_injection: Some(Arc::clone(&injection)),
        ready_path: ReadyPathTiming::default(),
        seed_history: None,
        source_releases: crate::product_host::SourceReleases::default(),
    };
    (config, injection, paths)
}

/// The overflow twin: the same host over a runtime that overflows its context.
fn product_overflow_attached_host(
    root: &std::path::Path,
    provider: NativeProvider,
) -> (
    HostConfig<surface_protocol::PlakExtension>,
    Arc<TestHostInjection>,
    PlakPaths,
) {
    let (mut config, _, paths) = product_injected_attached_host(root);
    let workspace = config.cwd.clone();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.join("overflow-inspect-evidence.jsonl"),
    )
    .unwrap();
    let target = overflow_runtime_binding(&workspace, controller.clone())
        .unwrap()
        .target_binding();
    let runtime_workspace = workspace.clone();
    let injection = Arc::new(TestHostInjection {
        runtime: Arc::new(move |_| {
            overflow_runtime_binding(&runtime_workspace, controller.clone())
                .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    config.session.account = Some(
        match provider {
            NativeProvider::Xai => "grok",
            NativeProvider::Claude => "claude",
            _ => "codex",
        }
        .into(),
    );
    config.target = HostTargetAuthority::Injected { provider };
    config.compaction_enabled = true;
    config.test_injection = Some(Arc::clone(&injection));
    (config, injection, paths)
}

/// Rows in one table of a durable authority; a database that was never opened
/// has none.
fn durable_authority_count(database: &std::path::Path, table: &str) -> i64 {
    if !database.exists() {
        return 0;
    }
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn send_attached_action(actions: &surface_protocol::UserActionSender, action: UserAction) {
    actions
        .send(action)
        .expect("the attached host accepts an action");
}

/// `/clear`, rebind and plan build a fresh spec, so the topology
/// gate is the file's answer *now* — not the one this attach started
/// with. `plak config set orchestration.subagents …` from another shell
/// is in force for the next recompile, and an unreadable roster refuses
/// the operation rather than freezing an epoch from the schema defaults.
/// Drive an attached host through `extra`, then quit, and collect what it
/// published.
fn run_attached_actions(
    config: HostConfig<surface_protocol::PlakExtension>,
    extra: Vec<UserAction>,
) -> Vec<HostEvent> {
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    for action in extra {
        send_attached_action(&actions, action);
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    events.try_iter().collect()
}

fn usage_statuses(events: &[HostEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Usage(usage) => Some(usage.status.as_str()),
            _ => None,
        })
        .collect()
}

/// Owners still holding a runtime branch; zero after a clean exit.
fn active_runtime_owner_count(database: &std::path::Path) -> i64 {
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .query_row(
            "SELECT COUNT(*) FROM branches WHERE active_owner IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn the_topology_gate_is_reread_from_the_file_on_every_recompile() {
    let root = tempfile::tempdir().unwrap();
    let (config, _injection, _paths) = product_injected_attached_host(root.path());
    assert!(
        config.topology_enabled,
        "an empty home starts from the schema default"
    );
    assert!(config.live_topology_enabled().unwrap());

    let mut saved = crate::product_config::ProductConfig::default_product();
    saved.subagents = false;
    saved
        .save(&PlakPaths::from_home(config.product_home.clone()))
        .unwrap();
    assert!(
        !config.live_topology_enabled().unwrap(),
        "the recompile reads the file, not the attach-time snapshot"
    );
    assert!(config.topology_enabled, "the attach-time fact is untouched");

    saved.subagents = true;
    saved
        .save(&PlakPaths::from_home(config.product_home.clone()))
        .unwrap();
    assert!(config.live_topology_enabled().unwrap());

    std::fs::write(
        PlakPaths::from_home(config.product_home.clone()).config_file(),
        "[orchestration\n",
    )
    .unwrap();
    assert!(
        config.live_topology_enabled().is_err(),
        "an unreadable roster refuses recompilation"
    );
}
#[test]
fn overflow_usage_and_codex_compact_records_capacity_recovery() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _, _paths) =
        product_overflow_attached_host(root.path(), NativeProvider::Codex);
    let session_id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let prepared = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&prepared);
    let observed_store = Arc::clone(&store);
    let observed_id = session_id.clone();
    Arc::get_mut(config.test_injection.as_mut().unwrap())
        .unwrap()
        .observe = Arc::new(move |workflow, boundary| {
        if workflow == TestHostWorkflow::Compact && boundary == TestHostBoundary::Prepared {
            *captured.lock().unwrap() = observed_store
                .load_intents(&observed_id)
                .unwrap()
                .workflow_requests;
        }
    });
    let events = run_attached_actions(
        config,
        vec![
            UserAction::Extension(SurfaceIntent::OpenUsage),
            UserAction::Compact,
        ],
    );
    let statuses = usage_statuses(&events);
    assert!(
        !statuses.is_empty()
            && statuses
                .iter()
                .all(|status| *status == "protected-overflow"),
        "empty-session Codex overflow must report protected-overflow, got {statuses:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::ContextCapacityRecovery(None))),
        "Codex native compact must not present protected-only recovery, events={events:?}"
    );
    let requests = prepared.lock().unwrap();
    assert!(
        requests.iter().any(|workflow| {
            matches!(
                workflow.kind,
                sessions::SessionWorkflowKind::Compact(
                    sessions::ProductCompactReason::CapacityRecovery
                )
            )
        }),
        "Codex overflow compact must persist capacity_recovery, requests={:?}",
        requests
    );

    assert!(store.load_views(&session_id).unwrap().is_some());
}
#[test]
fn scheduled_compaction_failure_keeps_host_worker_alive() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _, _paths) =
        product_overflow_attached_host(root.path(), NativeProvider::Codex);
    let workspace = config.cwd.clone();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("compact-fail-evidence.jsonl"),
    )
    .unwrap();
    let target = overflow_runtime_binding(&workspace, controller.clone())
        .unwrap()
        .target_binding();
    let runtime_workspace = workspace.clone();
    // The host mailbox coalesces `Status`/`SetBusy` to the latest value, so
    // the compacting row is observed while the compact runtime is being
    // resolved: the injected runtime parks until the test has drained.
    let (compact_requested_tx, compact_requested_rx) = std::sync::mpsc::channel::<()>();
    let (drained_tx, drained_rx) = std::sync::mpsc::channel::<()>();
    let drained_rx = std::sync::Mutex::new(drained_rx);
    config.test_injection = Some(Arc::new(TestHostInjection {
        runtime: Arc::new(move |role| {
            if role == TestRuntimeRole::Compact {
                let _ = compact_requested_tx.send(());
                let _ = drained_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
                return Err("injected compact runtime refused".into());
            }
            overflow_runtime_binding(&runtime_workspace, controller.clone())
                .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    }));
    let session_id = config.session.id.clone();
    let paths = PlakPaths::from_home(config.product_home.clone());
    let (actions, receiver, worker) = spawn_host_worker(config).unwrap();
    send_attached_action(
        &actions,
        UserAction::Prompt("continue after compact".into()),
    );
    compact_requested_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    let during: Vec<HostEvent> = receiver.try_iter().collect();
    drained_tx.send(()).unwrap();
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let after: Vec<HostEvent> = receiver.try_iter().collect();
    let events: Vec<HostEvent> = during.iter().chain(after.iter()).cloned().collect();
    assert!(
        events.iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("compact:")
        )),
        "scheduled compact Err must become ErrorNote, events={events:?}"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            HostEvent::Row(row) if row.text().is_some_and(|text| text.contains("host error:"))
        )),
        "scheduled compact Err must not kill the host worker, events={events:?}"
    );
    // The paid round-trip blocks the host; the TUI must see it as work and
    // see it end, or every deferred prompt looks like a frozen surface.
    assert!(
        during.iter().any(|event| matches!(
            event,
            HostEvent::Status { phase, .. } if phase == "compacting"
        )),
        "scheduled compact must paint compacting, during={during:?}"
    );
    assert!(
        during
            .iter()
            .any(|event| matches!(event, HostEvent::SetBusy(true))),
        "compacting row needs busy set, during={during:?}"
    );
    assert!(
        after
            .iter()
            .any(|event| matches!(event, HostEvent::SetBusy(false))),
        "a failed scheduled compact must clear busy, after={after:?}"
    );
    assert!(
        after.iter().any(|event| matches!(
            event,
            HostEvent::Status { phase, .. } if phase == "idle"
        )),
        "a failed scheduled compact must return the HUD to idle, after={after:?}"
    );
    let persisted = sessions::load_session(&paths, &session_id).unwrap();
    assert_eq!(
        persisted.queue_drain_state,
        sessions::QueueDrainState::RestoredPaused
    );
    assert!(
        persisted
            .pending_queue
            .iter()
            .any(|item| item.text == "continue after compact"),
        "failed scheduled compact must leave the prompt queued, queue={:?}",
        persisted.pending_queue
    );
}
#[test]
fn fresh_attached_quit_removes_unpublished_runtime_and_first_intent_publishes_pair() {
    let immediate_root = tempfile::tempdir().unwrap();
    let (config, _, paths) = product_injected_attached_host(immediate_root.path());
    let session_id = config.session.id.clone();
    assert_eq!(config.session.store_revision, 0);
    assert_eq!(
        durable_authority_count(&paths.product_database(), "sessions"),
        0
    );

    let (actions, _events, worker) = spawn_host_worker(config).unwrap();
    let ready_counts = (
        durable_authority_count(&paths.product_database(), "sessions"),
        durable_authority_count(&paths.runtime_database(), "sessions"),
    );
    assert_eq!(ready_counts, (0, 1));
    assert_eq!(active_runtime_owner_count(&paths.runtime_database()), 1);
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();

    assert_eq!(
        (
            durable_authority_count(&paths.product_database(), "sessions"),
            durable_authority_count(&paths.runtime_database(), "sessions"),
        ),
        (0, 0),
        "immediate quit must remove both unpublished product and runtime authority"
    );
    assert_eq!(
        active_runtime_owner_count(&paths.runtime_database()),
        0,
        "the unpublished exact runtime must still be released"
    );
    assert!(
        paths.session_events_file(&session_id).exists(),
        "ready paint opens the observation log at attach"
    );

    for _ in 0..2 {
        let (config, _, _) = product_injected_attached_host(immediate_root.path());
        let (actions, _events, worker) = spawn_host_worker(config).unwrap();
        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();
    }
    assert_eq!(
        durable_authority_count(&paths.product_database(), "sessions"),
        0
    );
    assert_eq!(
        durable_authority_count(&paths.runtime_database(), "sessions"),
        0
    );

    let intent_root = tempfile::tempdir().unwrap();
    let (config, injection, paths) = product_injected_attached_host(intent_root.path());
    let session_id = config.session.id.clone();
    run_injected_host(
        config,
        injection,
        vec![
            UserAction::Prompt("first durable intent".into()),
            UserAction::Quit,
        ],
    )
    .unwrap();

    assert_eq!(
        durable_authority_count(&paths.product_database(), "sessions"),
        1
    );
    assert_eq!(
        durable_authority_count(&paths.runtime_database(), "sessions"),
        1
    );
    assert_eq!(active_runtime_owner_count(&paths.runtime_database()), 0);
    let persisted = sessions::load_session(&paths, &session_id).unwrap();
    assert!(persisted.store_revision > 0);
    assert_eq!(persisted.prompt_history, ["first durable intent"]);
    let events = std::fs::read_to_string(paths.session_events_file(&session_id)).unwrap();
    let kinds = events
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|event| event.get("kind")?.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let positions = [
        "host.attached",
        "session.ready",
        "session.created",
        "session.quit",
    ]
    .map(|expected| {
        kinds
            .iter()
            .position(|kind| kind == expected)
            .unwrap_or_else(|| panic!("missing {expected} in {kinds:?}"))
    });
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "event order {positions:?} in {kinds:?}"
    );
}
#[test]
fn completed_terminal_outcome_remains_authoritative_when_exit_was_requested() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, injection, _paths) = product_injected_attached_host(root.path());
    let paths = PlakPaths::from_home(config.product_home.clone());
    let session_id = config.session.id.clone();
    let boundary = Arc::new((
        std::sync::Mutex::new((0_u8, false)),
        std::sync::Condvar::new(),
    ));
    let observed_boundary = Arc::clone(&boundary);
    config.test_injection = Some(Arc::new(TestHostInjection {
        runtime: Arc::clone(&injection.runtime),
        rebind_target: injection.rebind_target.clone(),
        observe: Arc::new(move |workflow, observed| {
            if workflow != TestHostWorkflow::SendNow {
                return;
            }
            let (state, changed) = &*observed_boundary;
            let mut state = state.lock().unwrap();
            match observed {
                TestHostBoundary::RuntimeReturned => {
                    state.0 = 1;
                    changed.notify_all();
                    while !state.1 {
                        state = changed.wait(state).unwrap();
                    }
                }
                TestHostBoundary::ProductPublished => state.0 = 2,
                TestHostBoundary::ProductCommitted => state.0 = 3,
                TestHostBoundary::Prepared | TestHostBoundary::TerminalReady => {}
            }
        }),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    }));
    let (actions, events, worker) = spawn_host_worker(config).unwrap();

    send_attached_action(
        &actions,
        UserAction::Prompt(COMPLETION_WINS_QUIT_PROMPT.into()),
    );
    // The gate is the boundary observation itself, never a sleep: the host
    // parks inside `RuntimeReturned` until this test releases it, so the wait
    // below ends exactly when the authoritative result exists. The deadline is
    // only a diagnosis for a host that never got there, and it is scaled by the
    // load the run is under -- a busy machine makes the turn slower, not the
    // ordering weaker (this is the 2026-09 flake: a fixed five seconds failed
    // at load while the ordering it pins never broke).
    let (state, changed) = &*boundary;
    let deadline = Instant::now() + crate::test_support::latency_budget(Duration::from_secs(5));
    let mut state = state.lock().unwrap();
    while state.0 < 1 {
        let timeout = deadline.saturating_duration_since(Instant::now());
        assert!(
            !timeout.is_zero(),
            "the host never reached RuntimeReturned; the authoritative result was not ready"
        );
        let waited = changed.wait_timeout(state, timeout).unwrap();
        state = waited.0;
    }
    send_attached_action(&actions, UserAction::Quit);
    state.1 = true;
    changed.notify_all();
    drop(state);
    worker.join().unwrap().unwrap();

    assert_eq!(
        boundary.0.lock().unwrap().0,
        3,
        "host exited before terminal publication"
    );
    let persisted = sessions::load_session(&paths, &session_id).unwrap();
    let assistant = persisted
        .transcript
        .iter()
        .filter(|line| matches!(line, TranscriptRow::Assistant { .. }))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        assistant,
        vec![TranscriptRow::assistant("loopback turn committed")]
    );
    assert!(
        persisted
            .transcript
            .iter()
            .all(|line| *line != TranscriptRow::Interrupted)
    );
    assert!(
        events
            .try_iter()
            .all(|event| event != HostEvent::Row(TranscriptRow::Interrupted))
    );
    assert_eq!(active_runtime_owner_count(&paths.runtime_database()), 0);
}
#[test]
fn quit_during_active_turn_waits_for_cancelled_terminal_projection() {
    let root = tempfile::tempdir().unwrap();
    let (config, _, _paths) = product_injected_attached_host(root.path());
    let paths = PlakPaths::from_home(config.product_home.clone());
    let session_id = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();

    send_attached_action(&actions, UserAction::Prompt("cancel before exit".into()));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if event == HostEvent::SetBusy(true) {
            break;
        }
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    let persisted = sessions::load_session(&paths, &session_id).unwrap();
    assert!(
        persisted.transcript.contains(&TranscriptRow::Interrupted),
        "quit must await and publish the authoritative cancelled result: {:?}",
        persisted.transcript
    );
    assert!(persisted.transcript.iter().any(
        |line| matches!(line, TranscriptRow::User { text, .. } if text == "cancel before exit")
    ));
}
/// A driver that
/// sends `Esc` then `q` back to back reached the quit branch after the
/// runtime had already published the cancelled terminal and dropped the
/// turn from its admitted set. The quit's own cancel was refused with
/// "turn is no longer admitted", the host read that as "the turn is still
/// running", abandoned the turn, and the session fell to
/// `terminal publication failed; continuation blocked`.
#[test]
fn interrupt_then_quit_publishes_the_cancelled_terminal() {
    let root = tempfile::tempdir().unwrap();
    let (config, _, _paths) = product_injected_attached_host(root.path());
    let paths = PlakPaths::from_home(config.product_home.clone());
    let session_id = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();

    send_attached_action(&actions, UserAction::Prompt("cancel before exit".into()));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if event == HostEvent::SetBusy(true) {
            break;
        }
    }
    send_attached_action(&actions, UserAction::Interrupt);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    let notes: Vec<String> = events
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(TranscriptRow::Notice(
                plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
            )) => Some(note),
            _ => None,
        })
        .collect();
    assert!(
        !notes.iter().any(|note| note.contains("no longer admitted")),
        "a turn the runtime has already terminalized is not still running: {notes:?}"
    );
    assert!(
        !notes.iter().any(|note| note.contains("quit abandoned")),
        "the quit must observe the cancelled terminal: {notes:?}"
    );
    let persisted = sessions::load_session(&paths, &session_id).unwrap();
    assert!(
        persisted.transcript.contains(&TranscriptRow::Interrupted),
        "the cancelled terminal must still be published: {:?}",
        persisted.transcript
    );
}

/// Spec 02 §5.5 Refresh readiness: a read that republishes the provider rows
/// and the template projection without mutating any credential.
#[test]
fn refresh_readiness_republishes_both_projections_without_touching_credentials() {
    use std::ffi::OsStr;
    let dir = tempfile::tempdir().unwrap();
    let paths =
        PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
    paths.ensure_layout().unwrap();
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    sessions::save_session(&paths, &mut session).unwrap();
    let (tx, rx, join) =
        crate::shell::host::spawn_shell_host(plakous_hosted::runtime::ShellHostConfig {
            surface: Some(ProductSurface::boxed(&paths)),
            binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
            subscriptions: crate::shell::host::open_product_subscriptions(),
            openrouter_credentials: crate::openrouter_product::ProductOpenRouterCredentials::shared(
                &paths,
            ),
            product_home: paths.home().to_path_buf(),
            home: paths.session_home(),
            store: Arc::new(crate::store::session_records::SqliteSessionStore::new(
                paths.clone(),
            )),
            session,
            cwd: dir.path().to_path_buf(),
            reason: "missing_defaults".into(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        });

    tx.send(UserAction::RefreshReadiness).unwrap();
    drop(tx);
    join.join().unwrap().unwrap();

    let observed = rx.try_iter().collect::<Vec<_>>();
    assert!(
        observed.iter().any(|event| matches!(
            event,
            HostEvent::ProviderSnapshot { providers, .. } if providers.len() == 7
        )),
        "every supported vertical is republished, events={observed:?}"
    );
    assert!(
        observed
            .iter()
            .any(|event| matches!(event, HostEvent::TemplateReadiness(_))),
        "readiness refresh also re-emits TemplateReadiness"
    );
    // A read never writes: no key file appeared.
    for account in ["openai", "xai", "anthropic", "openrouter"] {
        assert!(!paths.api_key_file(account).is_file());
    }
}

#[test]
fn clear_publishes_fresh_product_and_runtime_authority_before_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let paths = PlakPaths::explicit(home);
    paths.ensure_layout().unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("clear-evidence.jsonl"),
    )
    .unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let mut session = SessionRecord::new(workspace.clone());
    session.account = Some("codex".into());
    session.model = Some("offline-loopback-v1".into());
    session.effort = Some("medium".into());
    let initial_runtime = runtime_binding(&workspace, controller.clone()).unwrap();
    let initial_target = initial_runtime.target_binding();
    let operation = HostSessionOperationId::new(&session.create_operation_id).unwrap();
    let state_roots = crate::shell::host::product_state_roots(&paths);
    let mut opened = executor
        .block_on(build_or_open_injected_session(
            state_roots.runtime_sessions().unwrap(),
            None,
            None,
            operation,
            initial_runtime,
        ))
        .unwrap()
        .unwrap_or_else(|error| panic!("initial test session failed: {}", error.failure.message()));
    session.bind_plakous_locator(opened.locator()).unwrap();
    sessions::save_session(&paths, &mut session).unwrap();
    let rebind_request = plakous_hosted::sessions::projector::WorkflowRequest {
        operation_id: "plak:rebind:unadmitted-no-target".into(),
        source_locator: opened.locator(),
        kind: sessions::SessionWorkflowKind::Rebind(Box::new(sessions::RebindSelection {
            provider: "codex".into(),
            account: "codex".into(),
            model: "offline-loopback-v2".into(),
            effort: Some("medium".into()),
            composition: ProductComposition::FollowCatalog,
            target_selection: TargetSelection::Native,
            protocol: "responses".into(),
            route: "offline".into(),
            requested_model: "offline-loopback-v2".into(),
            selected_configuration: "selected".into(),
            effective_configuration: "effective".into(),
            account_binding: "account".into(),
        })),
    };
    let store = crate::store::session_records::SqliteSessionStore::new(paths.clone());
    plakous_hosted::session_store::SessionRecordStore::submit_workflow_request(
        &store,
        &mut session,
        &rebind_request,
    )
    .unwrap();
    let old_id = session.id.clone();
    let old_revision = session.store_revision;
    assert_eq!(
        durable_authority_count(&paths.product_database(), "sessions"),
        1
    );
    assert_eq!(
        durable_authority_count(&paths.runtime_database(), "sessions"),
        1
    );
    let resolutions = Arc::new(AtomicUsize::new(0));
    let resolution_counter = Arc::clone(&resolutions);
    let runtime_workspace = workspace.clone();
    let runtime_controller = controller.clone();
    let injection = Arc::new(TestHostInjection {
        runtime: Arc::new(move |_| {
            resolution_counter.fetch_add(1, Ordering::SeqCst);
            runtime_binding(&runtime_workspace, runtime_controller.clone())
                .map_err(|error| error.to_string())
        }),
        rebind_target: initial_target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let mut config = HostConfig {
        surface: Some(ProductSurface::boxed(&paths)),
        auto_continue: true,
        topology_admission: None,
        state_roots,
        product_home: paths.home().to_path_buf(),
        home: paths.session_home(),
        store: Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        )),
        session,
        target: HostTargetAuthority::Injected {
            provider: NativeProvider::Codex,
        },
        model_id: "offline-loopback-v1".into(),
        effort: Some(ReasoningEffort::Medium),
        effort_explicit: true,
        compaction_enabled: false,
        max_model_rounds: crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_enabled: SessionPolicy::load(&paths).unwrap().topology_enabled,
        root_template: None,
        attached_one_shot: false,
        interactive: true,
        cwd: workspace,
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        resume_publication: None,
        test_injection: Some(injection),
        ready_path: ReadyPathTiming::default(),
        seed_history: None,
        source_releases: crate::product_host::SourceReleases::default(),
    };
    let (event_tx, _event_rx) = surface_protocol::host_event_channel();
    let events = Sender::over(event_tx);
    clear_session_rebuild(
        &executor,
        &mut config,
        &mut opened,
        &mut Vec::new(),
        &events,
        None,
    )
    .unwrap();
    assert_ne!(config.session.id, old_id);
    assert_eq!(resolutions.load(Ordering::SeqCst), 1);
    assert_eq!(
        durable_authority_count(&paths.product_database(), "sessions"),
        2
    );
    assert_eq!(
        durable_authority_count(&paths.runtime_database(), "sessions"),
        2
    );
    let durable_target = sessions::load_session(&paths, &config.session.id).unwrap();
    assert!(durable_target.store_revision > 0);
    assert_eq!(durable_target.plakous_locator, Some(opened.locator()));
    assert_eq!(
        durable_target.composition,
        plakous_hosted::binding::CompositionSelection::FollowCatalog
    );
    let durable_source = sessions::load_session(&paths, &old_id).unwrap();
    assert!(durable_source.store_revision >= old_revision);
    assert_eq!(
        config
            .store
            .load_intents(&old_id)
            .unwrap()
            .workflow_request(&rebind_request.operation_id),
        Some(&rebind_request),
    );
    let mut store = crate::store::ProductStore::open(&paths).unwrap();
    let intents = store.load_intents(&old_id).unwrap();
    let clear = intents
        .workflow_requests
        .iter()
        .find(|request| request.kind == sessions::SessionWorkflowKind::Clear)
        .unwrap();
    assert_eq!(durable_target.id, clear.successor_product_id());
    let views = store.load_views(&old_id).unwrap().unwrap();
    assert!(views.state.workflows[&clear.operation_id].settled.is_some());
    assert_eq!(
        durable_target.create_operation_id,
        sessions::clear_create_operation_id(&clear.operation_id).unwrap()
    );
    assert_eq!(
        config.session.source_product_session_id.as_deref(),
        Some(old_id.as_str())
    );
    assert_eq!(
        config.session.source_workflow_operation_id.as_deref(),
        Some(clear.operation_id.as_str())
    );

    assert!(
        opened.agent().is_some(),
        "fresh root must remain prompt-ready"
    );
    executor.block_on(opened.release()).unwrap().unwrap();
}
