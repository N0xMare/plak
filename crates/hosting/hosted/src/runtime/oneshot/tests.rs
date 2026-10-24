#[test]
fn run_zero_timeout_stops_with_a_bound_not_a_successful_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(dir.path());
    let outcome = OneShot::run_until_idle(
        config,
        "task".into(),
        false,
        RunBounds {
            max_turns: 25,
            leave_jobs_running: false,
            deadline: Some(std::time::Instant::now()),
        },
        |_, _| {},
    )
    .unwrap();
    assert!(matches!(outcome, OneShotOutcome::Bounded { .. }));
}
use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

use super::*;
use crate::runtime::test_support::fresh_injected_attached_host;
use crate::sessions::{QueueDrainState, QueueItem};
use std::sync::Arc;

fn text_turn(
    prefix: &'static str,
    canonical: &'static str,
) -> (
    CliResult<OneShotOutcome>,
    Vec<HostEvent>,
    Vec<crate::sessions::TranscriptRow>,
) {
    drive_text_turn(prefix, canonical, false)
}

fn drive_text_turn(
    prefix: &'static str,
    canonical: &'static str,
    interrupt: bool,
) -> (
    CliResult<OneShotOutcome>,
    Vec<HostEvent>,
    Vec<crate::sessions::TranscriptRow>,
) {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let release = root.path().join("release");
    let controller =
        plakous_offline_crash_harness::BoundaryController::quiet_gated_from_evidence_path(
            plakous_offline_crash_harness::protocol::CrashBoundary::ProviderOutcomeOne,
            root.path().join("text-events.jsonl"),
            release.clone(),
        )
        .unwrap();
    let workspace = config.cwd.clone();
    let binding = plakous_offline_crash_harness::text_runtime_binding(
        &workspace,
        controller.clone(),
        prefix,
        canonical,
    )
    .unwrap();
    config.runtime_factory = Arc::new(super::super::ScriptedRuntimeFactory {
        rebind_target: binding.target_binding(),
        runtime: Arc::new(move |_| {
            plakous_offline_crash_harness::text_runtime_binding(
                &workspace,
                controller.clone(),
                prefix,
                canonical,
            )
            .map_err(|error| error.to_string())
        }),
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let store = config.store.clone();
    let id = config.session.id.clone();
    let mut observations = Vec::new();
    let result = OneShot::run(config, "text authority".into(), |event, controls| {
        if let Some(event) = event {
            if matches!(event, HostEvent::AssistantDelta(text) if text == prefix) {
                if interrupt {
                    controls.send(UserAction::Interrupt).unwrap();
                } else {
                    std::fs::write(&release, "release").unwrap();
                }
            }
            observations.push(event.clone());
        }
    });
    (
        result,
        observations,
        store.load(&id).unwrap().transcript.to_vec(),
    )
}

#[test]
fn streamed_prefix_is_completed_from_terminal_authority_once() {
    let (result, observations, transcript) = text_turn("first ", "first second");
    assert_eq!(
        result.unwrap(),
        OneShotOutcome::Completed {
            assistant: "first second".into()
        }
    );
    let text: String = observations
        .iter()
        .filter_map(|event| match event {
            HostEvent::AssistantDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "first second");
    assert_eq!(
        transcript
            .iter()
            .filter(|line| matches!(line, plakous_host_protocol::views::transcript::TranscriptRow::Assistant { text } if text == "first second"))
            .count(),
        1
    );
}

#[test]
fn divergent_preview_never_overrides_canonical_terminal_text() {
    let (result, observations, transcript) = text_turn("preview", "canonical answer");
    assert!(
        observations
            .iter()
            .any(|event| matches!(event, HostEvent::AssistantDelta(text) if text == "preview"))
    );
    assert_eq!(
        result.unwrap(),
        OneShotOutcome::Completed {
            assistant: "canonical answer".into()
        }
    );
    assert_eq!(
        transcript
            .iter()
            .filter(|line| matches!(line, plakous_host_protocol::views::transcript::TranscriptRow::Assistant { text } if text == "canonical answer"))
            .count(),
        1
    );
    assert!(!transcript.iter().any(|line| matches!(line, plakous_host_protocol::views::transcript::TranscriptRow::Assistant { text } if text == "preview")));
}

#[test]
fn nonempty_preview_does_not_hide_no_visible_terminal_text() {
    let (result, _, transcript) = text_turn("preview", "");
    assert_eq!(
        result.unwrap(),
        OneShotOutcome::Completed {
            assistant: String::new()
        }
    );
    assert!(!transcript.iter().any(|line| matches!(line, plakous_host_protocol::views::transcript::TranscriptRow::Assistant { text } if text == "preview")));
}

#[test]
fn settled_interruption_returns_the_committed_partial_projection() {
    let (result, _, transcript) = drive_text_turn("partial ", "complete answer", true);
    assert_eq!(
        result.unwrap(),
        OneShotOutcome::Interrupted {
            assistant: "partial ".into()
        }
    );
    // Spec 02 §2.6: streamed partial text is live only; the persisted
    // turn is its interrupted marker.
    assert!(
        !transcript
            .iter()
            .any(|line| line.text().is_some_and(|text| text.contains("partial"))),
        "{transcript:?}"
    );
    assert!(
        transcript.contains(&plakous_host_protocol::views::transcript::TranscriptRow::Interrupted)
    );
    assert!(
        !transcript
            .iter()
            .any(|line| matches!(line, plakous_host_protocol::views::transcript::TranscriptRow::Assistant { text } if text == "complete answer"))
    );
}

#[test]
fn completed_terminal_wins_a_quit_whose_cancellation_is_refused() {
    use super::super::{
        COMPLETION_WINS_QUIT_PROMPT, HostBoundary, HostWorkflow, ScriptedRuntimeFactory,
    };
    let root = tempfile::tempdir().unwrap();
    let (mut config, injection) = fresh_injected_attached_host(root.path());
    let boundary = Arc::new((std::sync::Mutex::new((0, false)), std::sync::Condvar::new()));
    let gate = boundary.clone();
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        runtime: injection.runtime.clone(),
        rebind_target: injection.rebind_target.clone(),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
        observe: Arc::new(move |workflow, boundary| {
            if workflow != HostWorkflow::SendNow {
                return;
            }
            let (state, wake) = &*gate;
            let mut state = state.lock().unwrap();
            match boundary {
                HostBoundary::RuntimeReturned => {
                    state.0 = 1;
                    while !state.1 {
                        state = wake.wait(state).unwrap();
                    }
                }
                HostBoundary::ProductPublished => state.0 = 2,
                HostBoundary::ProductCommitted => state.0 = 3,
                HostBoundary::Prepared | HostBoundary::TerminalReady => {}
            }
        }),
    });
    let result = OneShot::run(config, COMPLETION_WINS_QUIT_PROMPT.into(), |_, controls| {
        let (state, wake) = &*boundary;
        let mut state = state.lock().unwrap();
        if state.0 == 1 && !state.1 {
            controls.send(UserAction::Quit).unwrap();
            state.1 = true;
            wake.notify_all();
        }
    })
    .unwrap();
    assert_eq!(
        result,
        OneShotOutcome::Completed {
            assistant: "loopback turn committed".into()
        }
    );
    assert_eq!(
        boundary.0.lock().unwrap().0,
        3,
        "quit was handled before terminal publication"
    );
}

#[test]
fn one_shot_commits_releases_and_keeps_retained_work_paused() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config
        .session
        .pending_queue
        .push(QueueItem::new("retained work"));
    config.session.queue_drain_state = QueueDrainState::RestoredPaused;
    let store = config.store.clone();
    let id = config.session.id.clone();
    let mut observed = Vec::new();
    let result = OneShot::run(config, "one root turn".into(), |event, _| {
        if let Some(event) = event {
            observed.push(event.clone());
        }
    })
    .unwrap();
    assert_eq!(
        result,
        OneShotOutcome::Completed {
            assistant: "loopback turn committed".into()
        }
    );
    let stored = store.load(&id).unwrap();
    assert_eq!(stored.pending_queue.len(), 1);
    assert_eq!(stored.pending_queue[0].text, "retained work");
    assert_eq!(stored.queue_drain_state, QueueDrainState::RestoredPaused);
    assert!(stored.plakous_locator.is_some());
    assert_eq!(
        stored
            .transcript
            .iter()
            .filter(|line| matches!(line, plakous_host_protocol::views::transcript::TranscriptRow::Assistant { text } if text == "loopback turn committed"))
            .count(),
        1
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event, HostEvent::AssistantDone))
            .count(),
        1
    );
}

#[test]
fn one_shot_waits_past_the_interactive_bound_for_its_final_projection() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = config.store.clone();
    config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        store.clone(),
        crate::runtime::test_support::SlowProjection {
            delay: Duration::from_millis(600),
            refuse: false,
        },
    ));
    let id = config.session.id.clone();
    let result = OneShot::run(config, "one root turn".into(), |_, _| {}).unwrap();
    assert_eq!(
        result,
        OneShotOutcome::Completed {
            assistant: "loopback turn committed".into()
        }
    );
    let views = store.load_views(&id).unwrap().unwrap();
    assert!(
        views.rows.values().any(|row| matches!(row,
            plakous_host_protocol::views::transcript::TranscriptRow::Assistant { text }
                if text == "loopback turn committed")),
        "the one-shot exited before the projector applied its terminal"
    );
}

#[test]
fn one_shot_fails_when_its_final_projection_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        config.store.clone(),
        crate::runtime::test_support::SlowProjection {
            delay: Duration::ZERO,
            refuse: true,
        },
    ));
    let error = OneShot::run(config, "one root turn".into(), |_, _| {}).unwrap_err();
    assert!(
        error.to_string().contains("final projection failed"),
        "{error}"
    );
}

#[test]
fn canonical_release_failure_cannot_be_hidden_by_assistant_done() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, injection) = fresh_injected_attached_host(root.path());
    let database = config.product_home.join("runtime.sqlite3");
    config.runtime_factory = Arc::new(super::super::ScriptedRuntimeFactory {
        runtime: injection.runtime.clone(),
        rebind_target: injection.rebind_target.clone(),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
        observe: Arc::new(move |workflow, boundary| {
            if workflow == super::super::HostWorkflow::SendNow
                && boundary == super::super::HostBoundary::ProductCommitted
            {
                rusqlite::Connection::open(&database).unwrap().execute_batch(
                    "CREATE TRIGGER fail_release BEFORE UPDATE OF active_owner ON branches
                         WHEN NEW.active_owner IS NULL BEGIN SELECT RAISE(FAIL, 'injected release failure'); END;"
                ).unwrap();
            }
        }),
    });
    let mut done = false;
    let error = OneShot::run(config, "release failure".into(), |event, _| {
        done |= matches!(event, Some(HostEvent::AssistantDone));
    })
    .unwrap_err();
    assert!(done, "the committed assistant was observed before release");
    assert!(error.message().contains("release failed"), "{error:?}");
}
#[test]
fn parent_and_child_release_failures_are_both_retained() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, injection) = fresh_injected_attached_host(root.path());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let hub = executor
        .block_on(super::super::test_kit::install_failing_child_shutdown(
            &config.session.id,
            &config.session.workspace_root,
        ))
        .unwrap();
    let database = config.product_home.join("runtime.sqlite3");
    config.runtime_factory = Arc::new(super::super::ScriptedRuntimeFactory {
        runtime: injection.runtime.clone(),
        rebind_target: injection.rebind_target.clone(),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
        observe: Arc::new(move |workflow, boundary| {
            if workflow == super::super::HostWorkflow::SendNow
                && boundary == super::super::HostBoundary::ProductCommitted
            {
                rusqlite::Connection::open(&database).unwrap().execute_batch(
                    "CREATE TRIGGER fail_release BEFORE UPDATE OF active_owner ON branches
                         WHEN NEW.active_owner IS NULL BEGIN SELECT RAISE(FAIL, 'injected release failure'); END;"
                ).unwrap();
            }
        }),
    });
    let mut done = false;
    let error = OneShot::run(config, "release failure".into(), |event, _| {
        done |= matches!(event, Some(HostEvent::AssistantDone));
    })
    .unwrap_err();
    assert!(done, "the committed assistant was observed before release");
    assert!(
        error.message().contains("canonical session release failed"),
        "{error:?}"
    );
    assert!(
        error.message().contains("injected child release refusal"),
        "{error:?}"
    );
    hub.read(|tree| {
        assert_eq!(
            tree.fault_reason("c1"),
            Some("injected child release refusal")
        );
    })
    .unwrap();
}
#[test]
fn child_shutdown_failure_is_fatal_after_parent_release_and_locator_publication() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let store = config.store.clone();
    let id = config.session.id.clone();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let hub = executor
        .block_on(super::super::test_kit::install_failing_child_shutdown(
            &id,
            &config.session.workspace_root,
        ))
        .unwrap();
    let database = config.product_home.join("runtime.sqlite3");
    let mut done = false;
    let error = OneShot::run(config, "child release failure".into(), |event, _| {
        done |= matches!(event, Some(HostEvent::AssistantDone));
    })
    .unwrap_err();
    assert!(done);
    assert!(
        error.message().contains("injected child release refusal"),
        "{error:?}"
    );
    let stored = store.load(&id).unwrap();
    let locator = stored.plakous_locator.unwrap();
    let connection = rusqlite::Connection::open(database).unwrap();
    let checkpoint: String = connection
        .query_row(
            "SELECT checkpoint_id FROM branches WHERE session_id = ?1 AND branch_id = ?2",
            rusqlite::params![
                locator.session_id().to_string(),
                locator.branch_id().to_string()
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(checkpoint, locator.checkpoint_id().to_string());
    let held: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM branches WHERE active_owner IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(held, 0, "the parent must release despite child failure");
    hub.read(|tree| {
        assert_eq!(
            tree.node("c1").unwrap().status,
            crate::agent_tree_protocol::NodeStatus::RecoveryRequired
        );
        assert_eq!(
            tree.fault_reason("c1"),
            Some("injected child release refusal")
        );
    })
    .unwrap();
}

#[test]
fn quit_reports_a_stuck_child_release_and_still_publishes_the_parent_head() {
    stuck_child_quit_fixture(false);
}

#[test]
fn process_exit_quit_reports_a_stuck_child_release_and_still_publishes_the_parent_head() {
    stuck_child_quit_fixture(true);
}

fn stuck_child_quit_fixture(process_exit: bool) {
    use crate::runtime::startup::spawn_host_worker;
    use crate::runtime::test_support::send_attached_action;
    use std::time::Instant;

    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let store = config.store.clone();
    let id = config.session.id.clone();
    let database = config.product_home.join("runtime.sqlite3");
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let _hub = executor
        .block_on(super::super::test_kit::install_hanging_child_shutdown(
            &config.session.id,
            &config.session.workspace_root,
        ))
        .unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    // A committed turn publishes the session record first; quit then has
    // a saved parent locator to assert on.
    send_attached_action(&actions, UserAction::Prompt("commit before quit".into()));
    let admitted = Instant::now() + Duration::from_secs(5);
    loop {
        let event = events
            .recv_timeout(admitted.saturating_duration_since(Instant::now()))
            .expect("the committed turn must terminalize");
        if matches!(event, HostEvent::AssistantDone) {
            break;
        }
    }
    let started = Instant::now();
    if process_exit {
        actions.request_abandon_and_release();
    } else {
        send_attached_action(&actions, UserAction::Quit);
    }
    let result = worker.join().expect("host worker thread");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "quit waited out a stuck child: {elapsed:?}"
    );
    let error = result
        .expect_err("a child past its shutdown bound must fail the worker, not detach silently");
    assert!(error.message().contains("child retirement"), "{error:?}");
    let stored = store.load(&id).unwrap();
    let locator = stored.plakous_locator.unwrap();
    let connection = rusqlite::Connection::open(database).unwrap();
    let checkpoint: String = connection
        .query_row(
            "SELECT checkpoint_id FROM branches WHERE session_id = ?1 AND branch_id = ?2",
            rusqlite::params![
                locator.session_id().to_string(),
                locator.branch_id().to_string()
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(checkpoint, locator.checkpoint_id().to_string());
    let held: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM branches WHERE active_owner IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(held, 0, "the parent must release despite child failure");
}
