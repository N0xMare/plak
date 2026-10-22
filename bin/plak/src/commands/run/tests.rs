use super::*;
use crate::observation_parity_tests::{Fixture, Script};
use plakous_hosted::runtime::{HostBoundary, HostWorkflow, ScriptedRuntimeFactory};
use plakous_hosted::sessions::TranscriptRow;
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, text_runtime_binding};

#[test]
fn expired_run_timeout_precedes_authentication_preparation() {
    let home = tempfile::tempdir().unwrap();
    let mut app = PlakousApplication::for_test_accounts(home.path().join("state")).unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let outcome = app
        .run(
            &product_config::ProductConfig::default_product(),
            NativeProvider::Codex,
            "offline-model",
            None,
            None,
            "task".into(),
            false,
            &mut stdout,
            &mut stderr,
            product_host::RunBounds {
                max_turns: 25,
                leave_jobs_running: false,
                deadline: Some(std::time::Instant::now()),
            },
        )
        .unwrap();
    assert_eq!(outcome, ProcessOutcome::Bounded);
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(
        String::from_utf8(stderr).unwrap(),
        "plak: run stopped at timeout\n"
    );
}

#[test]
fn preparation_and_hosted_drive_share_one_absolute_run_deadline() {
    let fixture = Fixture::new(Script::Effect);
    let executor = ApplicationExecutor::new().unwrap();
    let limits = product_host::RunBounds {
        max_turns: 25,
        leave_jobs_running: false,
        deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(2)),
    };
    assert_eq!(
        run_preparation(&executor, limits, async { 42 }).unwrap(),
        Some(42)
    );
    assert!(
        run_preparation(&executor, limits, std::future::pending::<()>())
            .unwrap()
            .is_none()
    );
    let polled = std::cell::Cell::new(false);
    assert!(
        run_preparation(&executor, limits, async { polled.set(true) })
            .unwrap()
            .is_none()
    );
    assert!(
        !polled.get(),
        "expired preparation must not dispatch another request"
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = run_config(
        &executor,
        fixture.host_config(),
        "task".into(),
        false,
        false,
        face::ChildAgents::Available,
        TerminalMode::Piped,
        &mut stdout,
        &mut stderr,
        limits,
    )
    .unwrap();
    assert_eq!(result, ProcessOutcome::Bounded);
    assert_eq!(
        String::from_utf8(stderr)
            .unwrap()
            .matches("plak: run stopped at timeout")
            .count(),
        1
    );
}

#[test]
fn bounded_run_has_one_stop_diagnostic_and_a_distinct_exit_projection() {
    let fixture = Fixture::new(Script::Effect);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = run_config(
        &ApplicationExecutor::new().unwrap(),
        fixture.host_config(),
        "task".into(),
        false,
        false,
        face::ChildAgents::Available,
        TerminalMode::Piped,
        &mut stdout,
        &mut stderr,
        product_host::RunBounds {
            max_turns: 25,
            leave_jobs_running: false,
            deadline: Some(std::time::Instant::now()),
        },
    )
    .unwrap();
    assert_eq!(result, ProcessOutcome::Bounded);
    assert_eq!(
        String::from_utf8(stderr)
            .unwrap()
            .matches("plak: run stopped at timeout")
            .count(),
        1
    );
}

#[test]
fn production_one_shot_stdout_uses_terminal_text_once_and_retains_exact_transcript() {
    for (preview, canonical) in [
        ("first ", "first second"),
        ("preview", "canonical answer"),
        ("preview", ""),
    ] {
        let fixture = Fixture::new(Script::Effect);
        let mut config = fixture.host_config();
        let workspace = config.cwd.clone();
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            workspace.join("text-events.jsonl"),
        )
        .unwrap();
        let target = text_runtime_binding(&workspace, controller.clone(), preview, canonical)
            .unwrap()
            .target_binding();
        config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
            runtime: Arc::new(move |_| {
                text_runtime_binding(&workspace, controller.clone(), preview, canonical)
                    .map_err(|error| error.to_string())
            }),
            rebind_target: target,
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
        let store = config.store.clone();
        let id = config.session.id.clone();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = run_config(
            &ApplicationExecutor::new().unwrap(),
            config,
            "canonical stdout".into(),
            true,
            false,
            face::ChildAgents::Available,
            TerminalMode::Piped,
            &mut stdout,
            &mut stderr,
            plakous_hosted::runtime::RunBounds::default(),
        );
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            if canonical.is_empty() {
                format!("session_id={id}\n")
            } else {
                format!("session_id={id}\n{canonical}\n")
            }
        );
        let record = store.load(&id).unwrap();
        assert_eq!(result.unwrap(), ProcessOutcome::Success);
        if !canonical.is_empty() {
            assert_eq!(
                record
                    .transcript
                    .iter()
                    .filter(|line| matches!(line, TranscriptRow::Assistant { text } if *text == canonical))
                    .count(),
                1
            );
        }
        if preview != canonical {
            assert!(
                !record.transcript.iter().any(
                    |line| matches!(line, TranscriptRow::Assistant { text } if *text == preview)
                )
            );
        }
    }
}

/// A tool-bearing one-shot turn must reach its answer, through the real face.
///
/// The offline gate's two CLI cases hung here: the turn stopped after the first
/// tool result and never reached the second provider round. Nothing in the
/// suite covered a tool-bearing CLI turn *completing*, because the CLI signal
/// cases all cut their turn short, so the class was invisible. This drives the
/// `Script::Effect` runtime — round one calls `loopback_effect`, round two
/// answers — through `run_config` and the observe closure the product uses.
///
/// It runs on its own thread with a deadline so a regression fails the test
/// rather than hanging the suite.
#[test]
fn a_tool_bearing_one_shot_turn_reaches_its_answer_through_the_real_face() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let fixture = Fixture::new(Script::Effect);
        let mut config = fixture.host_config();
        // The product's own one-shot path has no surface: `run_hosted` in this
        // module builds its `HostConfig` with `surface: None`, and so does the
        // gate's offline hook. A fixture that keeps a surface is not testing
        // `plak run`.
        config.surface = None;
        // Two tool round trips, not one. `Script::Effect` calls a tool once,
        // and the hang needed a *second* dispatch after the first result, so a
        // one-tool script walked straight past it.
        let workspace = config.cwd.clone();
        let script_path = workspace.join("two-tool-script.json");
        std::fs::write(
            &script_path,
            br#"{"version":1,"steps":[
                {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
                {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
                {"output":{"text":"the offline answer is 42"},"usage":{"output":9}}]}"#,
        )
        .unwrap();
        let plan = Arc::new(
            plakous_offline_crash_harness::script::ScriptPlan::read(&script_path)
                .expect("the two-tool script is a valid plan"),
        );
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            workspace.join("two-tool-evidence.jsonl"),
        )
        .unwrap();
        let target = plakous_offline_crash_harness::scripted_runtime_binding(
            &workspace,
            controller.clone(),
            Arc::clone(&plan),
        )
        .unwrap()
        .target_binding();
        let scripted_workspace = workspace.clone();
        config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
            runtime: Arc::new(move |_| {
                plakous_offline_crash_harness::scripted_runtime_binding(
                    &scripted_workspace,
                    controller.clone(),
                    Arc::clone(&plan),
                )
                .map_err(|error| error.to_string())
            }),
            rebind_target: target,
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = run_config(
            &ApplicationExecutor::new().unwrap(),
            config,
            "drive one tool round trip".into(),
            false,
            false,
            face::ChildAgents::Available,
            TerminalMode::Piped,
            &mut stdout,
            &mut stderr,
            plakous_hosted::runtime::RunBounds::default(),
        );
        let _ = sender.send((
            result.map(|outcome| outcome == ProcessOutcome::Success),
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        ));
    });
    let (outcome, stdout, stderr) = receiver
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a tool-bearing one-shot turn must finish, not hang");
    worker.join().expect("the drive thread must not panic");
    assert_eq!(outcome.ok(), Some(true), "{stderr}");
    assert!(!stdout.is_empty(), "the turn produced no answer: {stderr}");
    assert!(
        stderr.contains("plak: tool done: loopback_effect"),
        "the tool never settled on the face: {stderr}"
    );
}

/// A turn that ran must never leave stderr empty.
///
/// The walkthrough's `--composition direct` run produced zero stderr lines for
/// a turn that really happened: the face rendered only tool lines and host
/// notes, and a turn whose model called no tool produced neither. The session
/// line is the fix, so this asserts the whole face through `run_config` rather
/// than the renderer alone.
#[test]
fn a_run_always_says_which_composition_it_used() {
    let fixture = Fixture::new(Script::Effect);
    let mut config = fixture.host_config();
    let workspace = config.cwd.clone();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        workspace.join("text-events.jsonl"),
    )
    .unwrap();
    let target = text_runtime_binding(&workspace, controller.clone(), "", "answered")
        .unwrap()
        .target_binding();
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        runtime: Arc::new(move |_| {
            text_runtime_binding(&workspace, controller.clone(), "", "answered")
                .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run_config(
        &ApplicationExecutor::new().unwrap(),
        config,
        "no tools at all".into(),
        false,
        false,
        face::ChildAgents::Available,
        TerminalMode::Piped,
        &mut stdout,
        &mut stderr,
        plakous_hosted::runtime::RunBounds::default(),
    )
    .unwrap();
    let face = String::from_utf8(stderr).unwrap();
    assert!(!face.is_empty(), "a turn that ran said nothing");
    let composition = face
        .lines()
        .find(|line| line.starts_with("plak: composition "))
        .expect("every run names its composition once");
    assert!(
        composition.contains("the model calls plak's tools"),
        "{composition}"
    );
    assert_eq!(
        face.matches("plak: composition ").count(),
        1,
        "the composition is announced once, not per republished projection"
    );
}

#[test]
fn failed_terminal_publication_never_reports_success_and_reopen_publishes_it() {
    for refused in [false, true] {
        let fixture = Fixture::new(Script::Effect);
        let mut config = if refused {
            crate::observation_parity_tests::refusal_config(&fixture, "")
        } else {
            fixture.host_config()
        };
        let injection = plakous_hosted::runtime::test_kit::take_scripted(&mut config);
        let paths = crate::home::paths::PlakPaths::explicit(config.product_home.clone());
        let database = paths.product_database();
        let failing_database = database.clone();
        config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
            runtime: injection.runtime.clone(),
            rebind_target: injection.rebind_target.clone(),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
            observe: Arc::new(move |workflow, boundary| {
                if workflow == HostWorkflow::RootDelivery && boundary == HostBoundary::TerminalReady
                {
                    let connection = rusqlite::Connection::open(&failing_database).unwrap();
                    connection.execute_batch("CREATE TRIGGER fail_terminal BEFORE UPDATE ON sessions BEGIN SELECT RAISE(FAIL, 'injected publication failure'); END;").unwrap();
                }
            }),
        });
        let store = config.store.clone();
        let id = config.session.id.clone();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = run_config(
            &ApplicationExecutor::new().unwrap(),
            config,
            "publication failure".into(),
            false,
            false,
            face::ChildAgents::Available,
            TerminalMode::Piped,
            &mut stdout,
            &mut stderr,
            plakous_hosted::runtime::RunBounds::default(),
        );
        let error = result.unwrap_err();
        assert!(
            error.message().contains("could not be committed")
                || error.message().contains("injected publication failure"),
            "{error:?}"
        );
        assert_eq!(stdout, [] as [u8; 0]);
        // The record lost only the terminal publication: the runtime head is
        // that turn's terminal. With storage restored, the next open adopts
        // exactly that head and publishes the skipped result, once.
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch("DROP TRIGGER fail_terminal;")
            .unwrap();
        for _ in 0..2 {
            let mut config = if refused {
                crate::observation_parity_tests::refusal_config(&fixture, "")
            } else {
                fixture.host_config()
            };
            config.session = store.load(&id).unwrap();
            let (actions, _events, worker) =
                plakous_hosted::runtime::spawn_host_worker(config).unwrap();
            actions
                .send(crate::surface_protocol::UserAction::Quit)
                .unwrap();
            worker.join().unwrap().unwrap();
            let record = store.load(&id).unwrap();
            let head = plakous::sessions::AgentSessions::inspect_recovery_sqlite(
                paths.runtime_database(),
                record.plakous_locator.unwrap(),
            );
            assert!(matches!(head, Ok(None)), "{head:?}");
            let published = record
                .transcript
                .iter()
                .filter(|line| line.text() == Some("loopback turn committed"))
                .count();
            assert_eq!(published, usize::from(!refused));
        }
    }
}

#[test]
fn failed_interactive_publication_recovers_in_place_on_the_next_prompt() {
    use crate::surface_protocol::{HostEvent, UserAction};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fixture = Fixture::new(Script::Effect);
    let mut config = fixture.host_config();
    let retained = plakous_hosted::sessions::QueueItem::new("retained queued prompt");
    config.session.pending_queue.push(retained.clone());
    let injection = plakous_hosted::runtime::test_kit::take_scripted(&mut config);
    let database =
        crate::home::paths::PlakPaths::explicit(config.product_home.clone()).product_database();
    let failing_database = database.clone();
    let terminals = Arc::new(AtomicUsize::new(0));
    let observed_terminals = terminals.clone();
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        runtime: injection.runtime.clone(),
        rebind_target: injection.rebind_target.clone(),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
        observe: Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::RootDelivery
                && boundary == HostBoundary::TerminalReady
                && observed_terminals.fetch_add(1, Ordering::SeqCst) == 0
            {
                rusqlite::Connection::open(&failing_database).unwrap().execute_batch(
                    "CREATE TRIGGER fail_terminal BEFORE UPDATE ON sessions BEGIN SELECT RAISE(FAIL, 'injected publication failure'); END;"
                ).unwrap();
            }
        }),
    });
    let store = config.store.clone();
    let id = config.session.id.clone();
    let (actions, events, worker) = plakous_hosted::runtime::spawn_host_worker(config).unwrap();
    actions
        .send(UserAction::Prompt("first turn".into()))
        .unwrap();
    loop {
        let event = events
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        if matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: ref note, .. })) if note.contains("product transcript could not be committed"))
        {
            break;
        }
    }
    // Storage recovers. The next prompt reopens the session in place and then
    // runs: the canonical conversation kept the lost turn, so nothing is
    // replayed, and nothing sends the operator to /clear. The retained queue
    // stays paused until it is resumed explicitly.
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch("DROP TRIGGER fail_terminal;")
        .unwrap();
    actions
        .send(UserAction::Prompt("after recovery".into()))
        .unwrap();
    let mut recovered = 0;
    loop {
        match events
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
        {
            HostEvent::Recovered { .. } => recovered += 1,
            HostEvent::Row(TranscriptRow::Notice(
                plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
            ))
            | HostEvent::Row(TranscriptRow::Notice(
                plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
            )) if note.contains("/clear") => {
                panic!("no dead end sends the operator to /clear: {note}")
            }
            HostEvent::AssistantDone => break,
            _ => {}
        }
    }
    assert_eq!(recovered, 1);
    assert_eq!(terminals.load(Ordering::SeqCst), 2);
    assert_eq!(store.load(&id).unwrap().pending_queue, vec![retained]);
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
}

#[test]
fn refusal_one_shot_cli_keeps_stdout_exact_and_returns_nonzero_after_publication() {
    for text in ["", "I cannot help with that."] {
        let fixture = Fixture::new(Script::Effect);
        let config = crate::observation_parity_tests::refusal_config(&fixture, text);
        let store = config.store.clone();
        let id = config.session.id.clone();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let failure = run_config(
            &ApplicationExecutor::new().unwrap(),
            config,
            "refusal fixture".into(),
            false,
            false,
            face::ChildAgents::Available,
            TerminalMode::Piped,
            &mut stdout,
            &mut stderr,
            plakous_hosted::runtime::RunBounds::default(),
        )
        .unwrap_err();
        assert_eq!(failure.exit_code(), 5);
        assert_eq!(failure.message(), "model refused the request");
        let expected = if text.is_empty() {
            Vec::new()
        } else {
            format!("{text}\n").into_bytes()
        };
        assert_eq!(stdout, expected);
        let stderr = String::from_utf8(stderr).unwrap();
        assert_eq!(stderr.matches("plak: model refused the request").count(), 0);
        assert!(!stderr.contains("empty stream"));
        let record = store.load(&id).unwrap();
        assert_eq!(
            record
                .transcript
                .iter()
                .filter(|line| matches!(line, TranscriptRow::Assistant { .. }))
                .count(),
            usize::from(!text.is_empty())
        );
        assert!(!record.transcript.iter().any(|line| {
            line.text()
                .is_some_and(|text| text.contains("model refused the request"))
        }));
    }
}

#[test]
fn refusal_pauses_queued_work_until_explicit_resume() {
    use crate::surface_protocol::{HostEvent, UserAction};
    let fixture = Fixture::new(Script::Effect);
    let workspace = fixture.host_config().cwd;
    let release = workspace.join("release-refusal");
    let evidence = workspace.join("refusal-events.jsonl");
    let controller = BoundaryController::quiet_gated_from_evidence_path(
        CrashBoundary::ProviderDispatchOne,
        &evidence,
        &release,
    )
    .unwrap();
    let config =
        crate::observation_parity_tests::refusal_config_with_controller(&fixture, "", controller);
    let store = config.store.clone();
    let id = config.session.id.clone();
    let (actions, events, worker) = plakous_hosted::runtime::spawn_host_worker(config).unwrap();
    let until = |predicate: &dyn Fn(&HostEvent) -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
            if predicate(&event) {
                break;
            }
        }
    };
    actions
        .send(UserAction::Prompt("first refusal".into()))
        .unwrap();
    until(&|event| matches!(event, HostEvent::SetBusy(true)));
    actions
        .send(UserAction::FollowUp("queued work".into()))
        .unwrap();
    until(
        &|event| matches!(event, HostEvent::QueueSnapshot { items, .. } if items.iter().any(|item| item.text == "queued work")),
    );
    std::fs::write(&release, "release").unwrap();
    until(&|event| matches!(event, HostEvent::ModelRefusal { .. }));
    // This read-only action acknowledges another host-loop iteration. It cannot
    // hide an already scheduled automatic second model dispatch.
    actions
        .send(UserAction::ResumeSession { id: None })
        .unwrap();
    until(&|event| matches!(event, HostEvent::ResumeSessions(_)));
    let record = store.load(&id).unwrap();
    assert_eq!(
        record.queue_drain_state,
        plakous_hosted::sessions::QueueDrainState::RestoredPaused
    );
    assert_eq!(record.pending_queue.len(), 1);
    let dispatches = || {
        plakous_offline_crash_harness::evidence::read_evidence(&evidence)
            .unwrap()
            .iter()
            .filter(|row| {
                row.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count()
    };
    assert_eq!(dispatches(), 1);
    actions.send(UserAction::ResumeQueueDrain).unwrap();
    until(&|event| matches!(event, HostEvent::ModelRefusal { .. }));
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
    assert_eq!(dispatches(), 2);
    assert_eq!(
        store.load(&id).unwrap().pending_queue,
        [] as [plakous_hosted::sessions::QueueItem; 0]
    );
}

#[test]
fn refusal_one_shot_release_failure_keeps_stdout_empty_and_reports_release() {
    let fixture = Fixture::new(Script::Effect);
    let mut config = crate::observation_parity_tests::refusal_config(&fixture, "refusal text");
    let injection = plakous_hosted::runtime::test_kit::take_scripted(&mut config);
    let database =
        crate::home::paths::PlakPaths::explicit(config.product_home.clone()).runtime_database();
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        runtime: injection.runtime.clone(),
        rebind_target: injection.rebind_target.clone(),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
        observe: Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::SendNow && boundary == HostBoundary::ProductCommitted {
                let connection = rusqlite::Connection::open(&database).unwrap();
                connection.execute_batch("CREATE TRIGGER fail_refused_release BEFORE UPDATE OF active_owner ON branches WHEN NEW.active_owner IS NULL BEGIN SELECT RAISE(FAIL, 'injected refusal release failure'); END;").unwrap();
            }
        }),
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let error = run_config(
        &ApplicationExecutor::new().unwrap(),
        config,
        "refused fixture".into(),
        false,
        false,
        face::ChildAgents::Available,
        TerminalMode::Piped,
        &mut stdout,
        &mut stderr,
        plakous_hosted::runtime::RunBounds::default(),
    )
    .unwrap_err();
    assert_eq!(error.exit_code(), 5);
    assert!(error.message().contains("release failed"), "{error:?}");
    assert_eq!(stdout, [] as [u8; 0]);
}

#[test]
fn one_shot_capacity_admission_failure_is_returned_without_duplicate_note() {
    let fixture = Fixture::new(Script::CapacityRefusal);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let failure = run_config(
        &ApplicationExecutor::new().unwrap(),
        fixture.host_config(),
        "capacity fixture".into(),
        false,
        false,
        face::ChildAgents::Available,
        TerminalMode::Piped,
        &mut stdout,
        &mut stderr,
        plakous_hosted::runtime::RunBounds::default(),
    )
    .unwrap_err();
    assert_eq!(failure.exit_code(), 5);
    assert_eq!(stdout, [] as [u8; 0]);
    let before_final = String::from_utf8(stderr.clone()).unwrap();
    assert!(!before_final.contains(failure.message()));
    assert!(failure.message().contains("compaction"), "{failure:?}");
}

#[test]
fn one_shot_terminal_error_is_returned_once_and_keeps_failure_guidance() {
    let fixture = Fixture::new(Script::Effect);
    let mut config = fixture.host_config();
    let workspace = config.cwd.clone();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        workspace.join("terminal-events.jsonl"),
    )
    .unwrap();
    let kind = plakous::model::ModelErrorKind::RateLimited;
    let target = plakous_offline_crash_harness::terminal_failure_runtime_binding(
        &workspace,
        controller.clone(),
        kind,
    )
    .unwrap()
    .target_binding();
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        runtime: Arc::new(move |_| {
            plakous_offline_crash_harness::terminal_failure_runtime_binding(
                &workspace,
                controller.clone(),
                kind,
            )
            .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let failure = run_config(
        &ApplicationExecutor::new().unwrap(),
        config,
        "terminal fixture".into(),
        false,
        false,
        face::ChildAgents::Available,
        TerminalMode::Piped,
        &mut stdout,
        &mut stderr,
        plakous_hosted::runtime::RunBounds::default(),
    )
    .unwrap_err();
    assert_eq!(failure.exit_code(), 5);
    assert_eq!(stdout, [] as [u8; 0]);
    let notes = String::from_utf8(stderr).unwrap();
    assert!(!notes.contains(failure.message()));
    assert!(
        notes.contains(
            "turn failed definitively; the durable session remains available for a new prompt"
        ),
        "{notes}"
    );
}

#[test]
fn one_shot_length_outputs_text_or_retries_empty_then_exits_five() {
    for visible in [false, true] {
        let fixture = Fixture::new(Script::Effect);
        let mut config = fixture.host_config();
        let workspace = config.cwd.clone();
        let evidence = workspace.join("length-events.jsonl");
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, evidence.clone())
                .unwrap();
        let steps = if visible {
            serde_json::json!([{"output": {"text": "truncated answer"}, "finish": "length"}])
        } else {
            serde_json::json!([
                {"output": "reasoning_only", "finish": "length"},
                {"output": "reasoning_only", "finish": "length"}
            ])
        };
        let plan: plakous_offline_crash_harness::script::ScriptPlan =
            serde_json::from_value(serde_json::json!({"version": 1, "steps": steps})).unwrap();
        let plan = Arc::new(plan);
        let target = plakous_offline_crash_harness::scripted_runtime_binding(
            &workspace,
            controller.clone(),
            Arc::clone(&plan),
        )
        .unwrap()
        .target_binding();
        config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
            runtime: Arc::new(move |_| {
                plakous_offline_crash_harness::scripted_runtime_binding(
                    &workspace,
                    controller.clone(),
                    Arc::clone(&plan),
                )
                .map_err(|e| e.to_string())
            }),
            rebind_target: target,
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let failure = run_config(
            &ApplicationExecutor::new().unwrap(),
            config,
            "length fixture".into(),
            false,
            false,
            face::ChildAgents::Available,
            TerminalMode::Piped,
            &mut stdout,
            &mut stderr,
            plakous_hosted::runtime::RunBounds::default(),
        )
        .unwrap_err();
        assert_eq!(failure.exit_code(), 5);
        let events = std::fs::read_to_string(evidence).unwrap();
        assert_eq!(
            events
                .lines()
                .filter(|line| line.contains("provider_dispatched"))
                .count(),
            if visible { 1 } else { 2 }
        );
        if visible {
            assert_eq!(
                String::from_utf8(stdout).unwrap().trim(),
                "truncated answer"
            );
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .contains("answer truncated at its")
            );
            assert_eq!(failure.message(), "turn_failed:output-limit");
        } else {
            assert_eq!(stdout, [] as [u8; 0]);
        }
    }
}

const HOSTILE: &str = "ok\u{1b}]52;c;cGF5bG9hZA==\u{7}\u{1b}[2J\tdone";

#[test]
fn stderr_face_lines_never_carry_control_sequences() {
    let mut face = face::CliFace::new(false, face::ChildAgents::Available);
    let lines = face.lines(&crate::surface_protocol::HostEvent::Row(
        TranscriptRow::error(
            plakous_host_protocol::views::transcript::NoticeScope::Host,
            format!("read_file failed: {HOSTILE}"),
        ),
    ));
    let mut stderr = Vec::new();
    for line in &lines {
        write_stderr_line(&mut stderr, line).unwrap();
    }
    assert_eq!(
        String::from_utf8(stderr).unwrap(),
        "plak: read_file failed: ok�]52;c;cGF5bG9hZA==��[2J\tdone\n"
    );
}

#[test]
fn assistant_stdout_ends_with_one_line_break_in_both_modes() {
    for mode in [TerminalMode::Interactive, TerminalMode::Piped] {
        assert!(assistant_stdout("a\r\nb".into(), mode).ends_with('\n'));
        assert_eq!(assistant_stdout(String::new(), mode), "");
    }
    assert_eq!(
        assistant_stdout("a\r\nb".into(), TerminalMode::Piped),
        "a\r\nb\n"
    );
    assert_eq!(
        assistant_stdout("a\r\nb".into(), TerminalMode::Interactive),
        "a\nb\n"
    );
}

/// `plak run` refuses a config file it does not fully understand before the
/// compatibility prompt, a credential read or any session state, exactly
/// like `plak agent`.
#[test]
fn run_refuses_an_invalid_config_before_any_account_or_session_work() {
    let home = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home.path().join("product"));
    paths.ensure_layout().unwrap();
    std::fs::write(paths.config_file(), "[tui]\nshow_timestamps = true\n").unwrap();
    let services = crate::shell::services::Services::for_home(paths.clone());
    let commands = [
        crate::command::Command::Run {
            limits: plakous_hosted::runtime::RunBounds::default(),
            provider: NativeProvider::Codex,
            model: "gpt-5.6-sol".into(),
            effort: None,
            composition: None,
            prompt: "must not dispatch".into(),
            print_session_id: false,
        },
        crate::command::Command::ContinueSession {
            limits: plakous_hosted::runtime::RunBounds::default(),
            session_id: "00000000-0000-0000-0000-000000000000".into(),
            prompt: "must not dispatch".into(),
            print_session_id: false,
            resume_tree: false,
        },
    ];
    for command in commands {
        let mut app = PlakousApplication::for_test_accounts(home.path().join("state")).unwrap();
        let mut stdin = io::Cursor::new(b"y\n".to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let failure = crate::execute_application_command(
            command,
            &services,
            &mut app,
            &mut stdin,
            &mut stdout,
            &mut stderr,
        )
        .unwrap_err();
        assert_eq!(failure.exit_code(), 1);
        assert!(
            failure.message().starts_with("invalid product config: "),
            "{}",
            failure.message()
        );
        assert!(
            failure.message().contains("show_timestamps"),
            "{}",
            failure.message()
        );
        assert_eq!(stdin.position(), 0, "the compatibility prompt must not run");
        assert_eq!(stdout, [] as [u8; 0]);
        assert_eq!(stderr, [] as [u8; 0]);
        assert!(!paths.product_database().exists());
    }
    assert!(!home.path().join("state").exists());
}

/// A signed-out subscription account is an auth failure (exit 3) that names
/// the login command. The state root is a temporary directory and the
/// account has no credential, so nothing reaches a provider.
#[test]
fn run_with_a_signed_out_subscription_exits_3_naming_plak_login() {
    let home = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home.path().join("product"));
    let services = crate::shell::services::Services::for_home(paths.clone());
    let mut app = PlakousApplication::for_test_accounts(home.path().join("state")).unwrap();
    let failure = crate::execute_application_command(
        crate::command::Command::Run {
            limits: plakous_hosted::runtime::RunBounds::default(),
            provider: NativeProvider::Codex,
            model: "gpt-5.6-sol".into(),
            effort: None,
            composition: None,
            prompt: "must not dispatch".into(),
            print_session_id: false,
        },
        &services,
        &mut app,
        &mut io::Cursor::new(b"y\n".to_vec()),
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert_eq!(failure.exit_code(), 3, "{}", failure.message());
    assert_eq!(
        failure.message(),
        "codex is signed out; run `plak login codex`"
    );
    assert!(!paths.product_database().exists());
}

/// A selection the provider refuses at resolution is the operator's to fix
/// (exit 2), like a model absent from the catalog; the rest keep their class.
#[test]
fn resolution_failures_keep_usage_auth_and_internal_classes_apart() {
    use plakous::subscriptions::NativeSubscriptionError as E;
    let home = tempfile::tempdir().unwrap();
    let app = PlakousApplication::for_test_accounts(home.path().join("state")).unwrap();
    for (error, exit) in [
        (E::ModelUnavailable, 2),
        (E::ProviderConfigurationRejected, 2),
        (E::ProviderConfigurationPolicyMismatch("synthetic"), 2),
        (E::ResolutionEvidenceRejected, 1),
        (E::Transport(None), 1),
        (E::Internal, 1),
        (E::CredentialUnavailable, 3),
        (E::AcceptanceRequired, 3),
    ] {
        let failure = app.resolution_failure(error, NativeProvider::Codex);
        assert_eq!(failure.exit_code(), exit, "{}", failure.message());
    }
}

/// Config warnings embed workspace-chosen paths, so a project directory named
/// with a terminal escape reaches stderr inert.
#[test]
fn config_warnings_reach_stderr_without_control_sequences() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("evil\u{1b}]0;owned\u{7}");
    std::fs::create_dir_all(project.join(".plak")).unwrap();
    std::fs::write(project.join(".plak/config.toml"), "").unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home.path().join("product"))
        .with_project_root(project);
    let mut stderr = Vec::new();
    super::super::support::admit_product_config(&paths, &mut stderr).unwrap();
    let stderr = String::from_utf8(stderr).unwrap();
    assert!(stderr.starts_with("warning: project "), "{stderr}");
    assert!(stderr.contains("evil\u{FFFD}]0;owned\u{FFFD}"), "{stderr}");
    assert!(
        !stderr.contains('\u{1b}') && !stderr.contains('\u{7}'),
        "{stderr}"
    );
}

/// The canonical answer as the product writes it: a terminal never receives
/// its control sequences, a pipe receives it byte for byte.
#[test]
fn assistant_stdout_is_sanitized_on_a_terminal_and_exact_on_a_pipe() {
    const ANSWER: &str = "ok\u{1b}]52;c;cGF5bG9hZA==\u{7}\u{1b}[2Jdone";
    for (mode, expected) in [
        (
            TerminalMode::Interactive,
            "ok\u{FFFD}]52;c;cGF5bG9hZA==\u{FFFD}\u{FFFD}[2Jdone\n",
        ),
        (
            TerminalMode::Piped,
            "ok\u{1b}]52;c;cGF5bG9hZA==\u{7}\u{1b}[2Jdone\n",
        ),
    ] {
        let fixture = Fixture::new(Script::Effect);
        let mut config = fixture.host_config();
        let workspace = config.cwd.clone();
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            workspace.join("text-events.jsonl"),
        )
        .unwrap();
        let target = text_runtime_binding(&workspace, controller.clone(), "", ANSWER)
            .unwrap()
            .target_binding();
        config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
            runtime: Arc::new(move |_| {
                text_runtime_binding(&workspace, controller.clone(), "", ANSWER)
                    .map_err(|error| error.to_string())
            }),
            rebind_target: target,
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = run_config(
            &ApplicationExecutor::new().unwrap(),
            config,
            "answer with escapes".into(),
            false,
            false,
            face::ChildAgents::Available,
            mode,
            &mut stdout,
            &mut stderr,
            plakous_hosted::runtime::RunBounds::default(),
        );
        let stderr = String::from_utf8(stderr).unwrap();
        assert_eq!(result.unwrap(), ProcessOutcome::Success, "{stderr}");
        assert_eq!(String::from_utf8(stdout).unwrap(), expected, "{mode:?}");
    }
}

#[test]
fn failed_terminal_then_successful_quit_handoff_replays_exactly_once() {
    use crate::surface_protocol::{HostEvent, UserAction};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fixture = Fixture::new(Script::Effect);
    let mut config = fixture.host_config();
    let retained = plakous_hosted::sessions::QueueItem::new("retained queued prompt");
    config.session.pending_queue.push(retained.clone());
    let injection = plakous_hosted::runtime::test_kit::take_scripted(&mut config);
    let database =
        crate::home::paths::PlakPaths::explicit(config.product_home.clone()).product_database();
    let failing_database = database.clone();
    let terminals = Arc::new(AtomicUsize::new(0));
    let observed_terminals = terminals.clone();
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        runtime: injection.runtime.clone(),
        rebind_target: injection.rebind_target.clone(),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
        observe: Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::RootDelivery
                && boundary == HostBoundary::TerminalReady
                && observed_terminals.fetch_add(1, Ordering::SeqCst) == 0
            {
                rusqlite::Connection::open(&failing_database).unwrap().execute_batch(
                    "CREATE TRIGGER fail_terminal BEFORE UPDATE ON sessions BEGIN SELECT RAISE(FAIL, 'injected publication failure'); END;"
                ).unwrap();
            }
        }),
    });
    let store = config.store.clone();
    let id = config.session.id.clone();
    let (actions, events, worker) = plakous_hosted::runtime::spawn_host_worker(config).unwrap();
    actions
        .send(UserAction::Prompt("first turn".into()))
        .unwrap();
    loop {
        let event = events
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        if matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: ref note, .. })) if note.contains("product transcript could not be committed"))
        {
            break;
        }
    }
    // Storage recovers, but no new turn repairs the projection: Quit saves
    // the handoff. Reopen must publish the terminal before settling it.
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch("DROP TRIGGER fail_terminal;")
        .unwrap();
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
    for _ in 0..2 {
        let mut config = fixture.host_config();
        config.session = store.load(&id).unwrap();
        let (actions, _, worker) = plakous_hosted::runtime::spawn_host_worker(config).unwrap();
        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();
        let record = store.load(&id).unwrap();
        assert_eq!(
            record
                .transcript
                .iter()
                .filter(|row| row.text() == Some("loopback turn committed"))
                .count(),
            1
        );
        assert_eq!(record.pending_queue, vec![retained.clone()]);
    }
    assert_eq!(terminals.load(Ordering::SeqCst), 1);
}

fn inprocess_finalization_recovery(explicit: bool) {
    use crate::surface_protocol::{HostEvent, UserAction};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let fixture = Fixture::new(Script::Effect);
    let mut config = fixture.host_config();
    let injection = plakous_hosted::runtime::test_kit::take_scripted(&mut config);
    let paths = crate::home::paths::PlakPaths::explicit(config.product_home.clone());
    let runtime_db = paths.runtime_database();
    let trigger_db = runtime_db.clone();
    let cut_once = std::sync::atomic::AtomicBool::new(false);
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        runtime: injection.runtime.clone(),
        rebind_target: injection.rebind_target.clone(),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
        observe: Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::RootDelivery
                && boundary == HostBoundary::Prepared
                && !cut_once.swap(true, Ordering::SeqCst)
            {
                rusqlite::Connection::open(&trigger_db).unwrap().execute_batch(
                    "CREATE TRIGGER stop_terminal BEFORE INSERT ON commit_receipts WHEN EXISTS (WITH RECURSIVE graph(kind,codec,digest) AS (SELECT root_kind,root_codec,root_digest FROM checkpoints WHERE checkpoint_id=NEW.checkpoint_id UNION SELECT child_kind,child_codec,child_digest FROM runtime_object_edges JOIN graph ON parent_kind=graph.kind AND parent_codec=graph.codec AND parent_digest=graph.digest) SELECT 1 FROM graph JOIN runtime_objects AS o USING(kind,codec,digest) JOIN runtime_object_bytes AS p ON p.id=o.id WHERE kind=8 AND instr(p.bytes, CAST('plakous-terminal-result' AS BLOB)) > 0) BEGIN SELECT RAISE(FAIL, 'injected terminal publication cut'); END;"
                ).unwrap();
            }
        }),
    });
    let store = config.store.clone();
    let id = config.session.id.clone();
    let (actions, events, worker) = plakous_hosted::runtime::spawn_host_worker(config).unwrap();
    let read_until = |predicate: fn(&HostEvent) -> bool| {
        let mut notes = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
            if let HostEvent::Row(row) = &event
                && let Some(text) = row.text()
            {
                notes.push(text.to_owned());
            }
            if predicate(&event) {
                return notes;
            }
        }
    };
    actions
        .send(UserAction::Prompt("first turn".into()))
        .unwrap();
    read_until(|event| matches!(event, HostEvent::SetBusy(true)));
    read_until(|event| matches!(event, HostEvent::SetBusy(false)));
    let mid = store.load(&id).unwrap();
    assert_eq!(
        mid.transcript
            .iter()
            .filter(|row| row.text() == Some("loopback turn committed"))
            .count(),
        0
    );
    let connection = rusqlite::Connection::open(&runtime_db).unwrap();
    connection
        .execute_batch("DROP TRIGGER stop_terminal;")
        .unwrap();
    let current: String = connection
        .query_row(
            "SELECT session_id || '/' || branch_id || '/' || checkpoint_id FROM branches",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let current = current.parse().unwrap();
    let recovery = plakous::sessions::AgentSessions::inspect_recovery_sqlite(&runtime_db, current)
        .unwrap()
        .unwrap();
    assert_eq!(
        recovery.phase(),
        plakous::sessions::SessionRecoveryPhase::Finalization
    );
    let control = if explicit {
        UserAction::Recover
    } else {
        UserAction::Prompt("second turn".into())
    };
    actions.send(control).unwrap();
    let notes = read_until(
        |event| matches!(event, HostEvent::Status { phase, detail: Some(detail), .. } if phase == "idle" && detail.starts_with("ready")),
    );
    assert!(
        !notes
            .iter()
            .any(|note| note.contains("left nothing outstanding")),
        "misreported the published terminal: {notes:?}"
    );
    if !explicit {
        read_until(|event| matches!(event, HostEvent::AssistantDone));
    }
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
    let record = store.load(&id).unwrap();
    assert_eq!(
        record
            .transcript
            .iter()
            .filter(|row| row.text() == Some("loopback turn committed"))
            .count(),
        if explicit { 1 } else { 2 },
        "the first turn's saved answer was never published"
    );
    assert!(
        plakous::sessions::AgentSessions::inspect_recovery_sqlite(
            &runtime_db,
            record.plakous_locator.unwrap()
        )
        .unwrap()
        .is_none()
    );
    // A second host observes the published binding, never a second copy.
    let mut config = fixture.host_config();
    config.session = record;
    let (actions, _, worker) = plakous_hosted::runtime::spawn_host_worker(config).unwrap();
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
    let record = store.load(&id).unwrap();
    assert_eq!(
        record
            .transcript
            .iter()
            .filter(|row| row.text() == Some("loopback turn committed"))
            .count(),
        if explicit { 1 } else { 2 }
    );
}

#[test]
fn inprocess_finalization_next_prompt_publishes_answer_once() {
    inprocess_finalization_recovery(false);
}

#[test]
fn inprocess_finalization_explicit_recover_publishes_answer_once() {
    inprocess_finalization_recovery(true);
}

#[test]
fn recorded_terminal_uncertain_siblings_reopen_without_notice_or_second_terminal() {
    use crate::surface_protocol::{HostEvent, UserAction};
    use plakous_host_protocol::views::transcript::NoticeScope;
    use plakous_offline_crash_harness::ledger_signature::{
        inspect_completed_root_answers, inspect_ledger, inspect_provider_owners,
    };

    let fixture = Fixture::new(Script::Effect);
    let mut config = fixture.host_config();
    let store = config.store.clone();
    let id = config.session.id.clone();
    let paths = crate::home::paths::PlakPaths::explicit(config.product_home.clone());
    let failing_database = paths.product_database();
    let injection = plakous_hosted::runtime::test_kit::take_scripted(&mut config);
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        runtime: injection.runtime.clone(),
        rebind_target: injection.rebind_target.clone(),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
        observe: Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::RootDelivery && boundary == HostBoundary::TerminalReady {
                rusqlite::Connection::open(&failing_database).unwrap().execute_batch(
                    "CREATE TRIGGER fail_terminal BEFORE UPDATE ON sessions BEGIN SELECT RAISE(FAIL, 'injected publication failure'); END;",
                ).unwrap();
            }
        }),
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run_config(
        &ApplicationExecutor::new().unwrap(),
        config,
        "recorded answer".into(),
        false,
        false,
        face::ChildAgents::Available,
        TerminalMode::Piped,
        &mut stdout,
        &mut stderr,
        plakous_hosted::runtime::RunBounds::default(),
    )
    .unwrap_err();
    rusqlite::Connection::open(paths.product_database())
        .unwrap()
        .execute_batch("DROP TRIGGER fail_terminal;")
        .unwrap();
    // The record save failed, but the answer is a fact: the projector's
    // views hold it once (spec 02 §2.6).
    let original = store.load(&id).unwrap();
    assert_eq!(
        original
            .transcript
            .iter()
            .filter(|row| row.text() == Some("loopback turn committed"))
            .count(),
        1
    );
    let source: String = rusqlite::Connection::open(paths.runtime_database())
        .unwrap()
        .query_row(
            "SELECT session_id || '/' || branch_id || '/' || checkpoint_id FROM branches",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let source = source.parse().unwrap();
    let owners = inspect_provider_owners(&paths.runtime_database(), source, "loopback").unwrap();
    let plakous::model::ModelOperationRoot::Turn(turn_id) = owners.last().unwrap().owner else {
        panic!("root provider");
    };
    let debt = ApplicationExecutor::new()
        .unwrap()
        .block_on(
            plakous_offline_crash_harness::seed_uncertain_terminal_siblings(
                &paths.runtime_database(),
                source,
                turn_id,
            ),
        )
        .unwrap()
        .unwrap();
    let before = inspect_ledger(&paths.runtime_database(), debt, "loopback").unwrap();
    assert_eq!(
        before
            .iter()
            .filter(|record| record.state == "dispatched")
            .count(),
        2
    );
    for _ in 0..2 {
        let mut config = fixture.host_config();
        config.session = store.load(&id).unwrap();
        let (actions, events, worker) = plakous_hosted::runtime::spawn_host_worker(config).unwrap();
        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();
        let seen = events.try_iter().collect::<Vec<_>>();
        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, HostEvent::AssistantDone))
                .count(),
            0
        );
        assert_eq!(
            seen.iter()
                .filter(
                    |event| matches!(event, HostEvent::Row(TranscriptRow::Notice(row))
                    if row.scope == NoticeScope::Recovery)
                )
                .count(),
            0
        );
        let record = store.load(&id).unwrap();
        assert_eq!(
            record
                .transcript
                .iter()
                .filter(|row| row.text() == Some("loopback turn committed"))
                .count(),
            1
        );
        assert_eq!(
            record
                .transcript
                .iter()
                .filter(|row| matches!(row, TranscriptRow::Notice(row)
                    if row.scope == NoticeScope::Recovery))
                .count(),
            0
        );
        let after = inspect_ledger(
            &paths.runtime_database(),
            record.plakous_locator.unwrap(),
            "loopback",
        )
        .unwrap();
        assert!(after.iter().all(|record| record.state == "finalized"));
        assert_eq!(
            inspect_completed_root_answers(
                &paths.runtime_database(),
                record.plakous_locator.unwrap()
            )
            .unwrap()
            .len(),
            1
        );
    }
}
