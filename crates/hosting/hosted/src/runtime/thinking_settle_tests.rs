//! Reasoning settles per model response on the Content view the host drives:
//! a discarding replay drops only its own response, and every way a turn ends
//! keeps what earlier responses showed.

use crate::runtime::recover_tests::{script, scripted_host_with, turn_completed, wait_for};
use crate::runtime::spawn_host_worker;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::send_attached_action;
use crate::sessions;
use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
use std::sync::Arc;

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

/// The persisted thinking and tool rows, in transcript order.
fn activity_rows(record: &sessions::SessionRecord) -> Vec<String> {
    record
        .transcript
        .rows()
        .into_iter()
        .filter_map(|(line, _)| {
            line.as_thinking()
                .map(|thinking| format!("think:{}", thinking.text))
                .or_else(|| line.as_tool().map(|tool| format!("tool:{}", tool.name)))
        })
        .collect()
}

fn reasoning_shown(text: &'static str) -> impl Fn(&HostEvent) -> bool {
    move |event| matches!(event, HostEvent::ReasoningDelta { text: shown, .. } if shown == text)
}

/// Round one thinks and calls a tool; round two's request is replayed after
/// it streamed reasoning. The reopened transcript keeps round one's thinking
/// before its tool and only the replay's thinking after it.
#[test]
fn a_discarding_replay_keeps_earlier_rounds_thinking_in_order() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([
            {
                "stream": [{"reasoning": "think one"}],
                "output": {"tool": {"name": "loopback_effect", "arguments": {}}},
                "finish": "tool_calls"
            },
            {
                "stream": [
                    {"reasoning": "think two"},
                    "retry_discarded",
                    {"reasoning": "think two replayed"}
                ],
                "output": {"text": "done"}
            }
        ])),
    );
    let session_id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("think".into()));
    wait_for(&events, &mut seen, "the completed turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    assert!(
        seen.iter()
            .any(|event| matches!(event, HostEvent::ModelRetry(retry) if retry.output_discarded)),
        "the replay was observed: {seen:#?}"
    );
    // Persisted rows derive from committed history (spec 02 §2.6): this
    // script streams its reasoning without committing it, so only the tool
    // row persists; the live order is the reasoning events above.
    assert_eq!(
        activity_rows(&store.load(&session_id).unwrap()),
        ["tool:loopback_effect"]
    );
}

/// A discard in round two then an interrupt keeps round one's text in the
/// interrupted row; only the discarded request's text is gone.
#[test]
fn a_discard_then_cancel_keeps_earlier_rounds_text() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([
            {
                "prefix": "round one. ",
                "output": {"tool": {"name": "loopback_effect", "arguments": {}}},
                "finish": "tool_calls"
            },
            {
                "stream": [
                    {"text": "doomed"},
                    "retry_discarded",
                    {"text": "replayed"},
                    {"reasoning": "replay thinking"},
                    {"stall": 30000}
                ]
            }
        ])),
    );
    let session_id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("go".into()));
    wait_for(
        &events,
        &mut seen,
        "the replay's reasoning",
        reasoning_shown("replay thinking"),
    );
    send_attached_action(&actions, UserAction::Interrupt);
    wait_for(&events, &mut seen, "the interrupted marker", |event| {
        matches!(
            event,
            HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Interrupted)
        )
    });
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    let record = store.load(&session_id).unwrap();
    let assistant = record
        .transcript
        .rows()
        .into_iter()
        .filter_map(|(line, _)| match line {
            sessions::TranscriptRow::Assistant { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    // The interrupted round's streamed text and reasoning are live only
    // (spec 02 §2.6).
    assert!(
        assistant.iter().all(|text| !text.contains("replayed")),
        "{assistant:?}"
    );
    assert_eq!(activity_rows(&record), ["tool:loopback_effect"]);
}

/// Quit after a tool round is bounded even when the provider ignores cancel;
/// reasoning the provider never committed remains live only.
#[test]
fn a_quit_after_a_tool_round_is_bounded_and_keeps_uncommitted_thinking_live_only() {
    let root = tempfile::tempdir().unwrap();
    let mut plan = serde_json::json!({
        "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
        "stall_ignores_cancellation": true,
        "steps": [
            {
                "stream": [{"reasoning": "think one"}],
                "output": {"tool": {"name": "loopback_effect", "arguments": {}}},
                "finish": "tool_calls"
            },
            {"stream": [{"reasoning": "think two"}, {"stall": 9000}]}
        ]
    });
    let (config, _) = scripted_host_with(
        root.path(),
        Arc::new(serde_json::from_value(plan.take()).unwrap()),
    );
    let session_id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("go".into()));
    wait_for(
        &events,
        &mut seen,
        "round two's reasoning",
        reasoning_shown("think two"),
    );
    let quit_started = std::time::Instant::now();
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let quit_elapsed = quit_started.elapsed();
    assert!(
        quit_elapsed < std::time::Duration::from_secs(1),
        "quit waited {quit_elapsed:?} for a deaf provider instead of abandoning ownership"
    );
    seen.extend(events.try_iter());

    assert!(
        seen.iter().any(|event| matches!(event,
            HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("quit escalated a cancel that did not settle"))),
        "the quit abandoned the turn: {seen:#?}"
    );
    // Streamed reasoning the script never commits is live only.
    assert_eq!(
        activity_rows(&store.load(&session_id).unwrap()),
        ["tool:loopback_effect"]
    );
}

#[test]
fn quit_grace_keeps_a_settled_interrupted_terminal_and_publishes_locator() {
    quit_grace_fixture(false, QuitIntent::ProcessExit, false);
}

#[test]
fn quit_grace_abandons_unsettled_cancel_and_reopens_the_projected_head() {
    quit_grace_fixture(true, QuitIntent::ProcessExit, false);
}

#[test]
fn ordinary_quit_keeps_a_settled_interrupted_terminal_and_publishes_locator() {
    quit_grace_fixture(false, QuitIntent::Explicit, false);
}

#[test]
fn ordinary_quit_abandons_unsettled_cancel_and_reopens_the_projected_head() {
    quit_grace_fixture(true, QuitIntent::Explicit, false);
}

#[test]
fn ordinary_quit_with_settled_cancel_reports_a_stuck_child_release() {
    quit_grace_fixture(false, QuitIntent::Explicit, true);
}

#[test]
fn process_exit_quit_with_settled_cancel_reports_a_stuck_child_release() {
    quit_grace_fixture(false, QuitIntent::ProcessExit, true);
}

#[test]
fn disconnected_control_plane_keeps_a_settled_interrupted_terminal() {
    quit_grace_fixture(false, QuitIntent::Disconnected, false);
}

#[test]
fn disconnected_control_plane_abandons_unsettled_cancel_within_quit_budget() {
    quit_grace_fixture(true, QuitIntent::Disconnected, false);
}

#[derive(Debug)]
enum QuitIntent {
    Explicit,
    ProcessExit,
    Disconnected,
}

fn quit_grace_fixture(deaf: bool, quit: QuitIntent, stuck_child: bool) {
    let root = tempfile::tempdir().unwrap();
    let plan = serde_json::json!({
        "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
        "stall_ignores_cancellation": deaf,
        "steps": [{"stream": [{"text": "quit grace prefix"}, {"stall": 30000}]}]
    });
    let plan: Arc<plakous_offline_crash_harness::script::ScriptPlan> =
        Arc::new(serde_json::from_value(plan).unwrap());
    let (config, _) = scripted_host_with(root.path(), plan.clone());
    let session_id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let workspace = config.cwd.clone();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let _hub = stuck_child.then(|| {
        executor
            .block_on(super::test_kit::install_hanging_child_shutdown(
                &session_id,
                &workspace,
            ))
            .unwrap()
    });
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("start".into()));
    wait_for(
        &events,
        &mut seen,
        "assistant prefix",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("quit grace prefix")),
    );
    send_attached_action(&actions, UserAction::FollowUp("retained follow-up".into()));
    wait_for(
        &events,
        &mut seen,
        "queued follow-up",
        |event| matches!(event, HostEvent::PromptAdmitted(item) if item.text == "retained follow-up"),
    );
    let quit_started = std::time::Instant::now();
    match quit {
        QuitIntent::ProcessExit => actions.request_abandon_and_release(),
        QuitIntent::Explicit => send_attached_action(&actions, UserAction::Quit),
        QuitIntent::Disconnected => drop(actions),
    }
    let result = worker.join().unwrap();
    if stuck_child {
        let error = result.expect_err("a settled quit must report failed child retirement");
        assert!(error.message().contains("child retirement"), "{error:?}");
    } else {
        result.unwrap();
    }
    let quit_elapsed = quit_started.elapsed();
    assert!(
        quit_elapsed < std::time::Duration::from_secs(if stuck_child { 5 } else { 1 }),
        "quit waited {quit_elapsed:?} (deaf={deaf}, quit={quit:?})"
    );
    seen.extend(events.try_iter());
    let interrupted = seen.iter().any(|event| {
        matches!(
            event,
            HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Interrupted)
        )
    });
    assert_eq!(interrupted, !deaf, "{seen:#?}");
    let abandoned = seen.iter().any(|event| {
        matches!(event, HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
        if note.contains("quit escalated a cancel that did not settle") && note.contains("300ms"))
    });
    assert_eq!(abandoned, deaf, "{seen:#?}");
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.pending_queue.len(), 1);
    assert_eq!(record.pending_queue[0].text, "retained follow-up");
    assert_eq!(
        record.queue_drain_state,
        sessions::QueueDrainState::RestoredPaused
    );
    assert_eq!(
        record
            .transcript
            .rows()
            .iter()
            .any(|(line, _)| **line == sessions::TranscriptRow::Interrupted),
        !deaf
    );
    let views = store.load_views(&session_id).unwrap().unwrap();
    assert_eq!(
        views.state.released, !stuck_child,
        "a late joined release leaves projection debt within the single quit budget"
    );
    // Process-exit abandonment intentionally leaves Working for recovery.
    assert_eq!(views.state.head_working, deaf);
    assert_eq!(views.state.open_turns.is_empty(), !deaf);
    let tail = executor
        .block_on(sessions.feed_page(FeedPageRequest::after(FeedCursor::new(
            record.plakous_locator.unwrap().session_id(),
            views.cursor,
        ))))
        .unwrap()
        .unwrap();
    if stuck_child {
        assert!(
            matches!(tail, FeedPage::Records { records, tail, .. }
            if !records.is_empty() && tail > views.cursor),
            "late release must leave its unpaid feed records"
        );
    } else {
        assert!(
            matches!(tail, FeedPage::Records { records, tail, .. }
            if records.is_empty() && tail == views.cursor),
            "quit left release records behind the cursor"
        );
    }
    executor
        .block_on(async {
            let locator = super::projection::project_before_open(&sessions, &store, &record)
                .await
                .unwrap()
                .unwrap();
            let paid = store.load_views(&session_id).unwrap().unwrap();
            assert!(paid.state.released, "next open must apply a late lease release");
            let tail = sessions.feed_page(FeedPageRequest::after(FeedCursor::new(locator.session_id(), paid.cursor))).await.unwrap();
            assert!(matches!(tail, FeedPage::Records { records, tail, .. } if records.is_empty() && tail == paid.cursor));
            let controller =
                plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::None,
                    root.path().join("quit-reopen-evidence.jsonl"),
                )
                .unwrap();
            let binding = plakous_offline_crash_harness::scripted_runtime_binding(
                &workspace, controller, plan,
            )
            .unwrap();
            let plakous::sessions::OpenSessionOutcome::Opened { session, .. } = sessions
                .open(plakous::sessions::OpenSessionRequest::new(
                    plakous::sessions::SessionOperationId::new("quit-grace:exact-open").unwrap(),
                    locator,
                    binding,
                ))
                .await
                .unwrap()
            else {
                panic!("the projected head must open exactly")
            };
            assert_eq!(session.agent().abandoned_turn().is_some(), deaf);
            session.release().await.unwrap();
        })
        .unwrap();
}
