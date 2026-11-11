#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

use super::*;

use crate::interrupt::{InterruptWatch, InterruptWatchState};
use crate::runtime::test_support::*;
use plakous::application::ApplicationExecutor;
use std::cell::RefCell;

#[test]
fn fork_progress_reports_applied_and_tail_sequences() {
    use plakous::sessions::feed::FeedSeq;
    let (events, observed) = host_event_sink();
    fork_progress(&events, FeedSeq::new(2), FeedSeq::new(5));
    assert!(
        matches!(observed.try_recv().unwrap(), HostEvent::Activity { label, active: true }
        if label == "preparing fork: 2/5")
    );
    end_fork_progress(&events);
    assert!(matches!(
        observed.try_recv().unwrap(),
        HostEvent::Activity { active: false, .. }
    ));
}

#[test]
fn interrupt_at_canonical_return_saves_resumable_fork_and_allows_another_fork() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, injection) = fresh_injected_attached_host(root.path());
    let mut opened = executor
        .block_on(super::super::startup::build_session_from_config(
            &mut config,
            &mut Vec::new(),
        ))
        .unwrap()
        .unwrap();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    let source_id = config.session.id.clone();
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let sent = AtomicBool::new(false);
    config.runtime_factory = Arc::new(crate::runtime::ScriptedRuntimeFactory {
        runtime: Arc::clone(&injection.runtime),
        rebind_target: injection.rebind_target.clone(),
        observe: Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::Fork
                && boundary == HostBoundary::RuntimeReturned
                && !sent.swap(true, Ordering::SeqCst)
            {
                actions.send(UserAction::Interrupt).unwrap();
            }
        }),
        child_runtime: None,
        profile_route: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    let (events, _rx) = host_event_sink();
    let error = executor
        .block_on(handle_fork(
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        ))
        .expect("the executor drives the call")
        .unwrap_err();
    assert!(
        error.message().contains("fork stopped; saved as session"),
        "{error:?}"
    );
    assert!(!error.message().contains("could not project"));
    assert_eq!(config.session.id, source_id);
    assert!(opened.agent().is_some());
    let request = config
        .store
        .load_intents(&source_id)
        .unwrap()
        .workflow_requests[0]
        .clone();
    let views = config.store.load_views(&source_id).unwrap().unwrap();
    assert!(
        views.state.workflows[&request.operation_id]
            .settled
            .is_some()
    );
    let target = config.store.load(&request.successor_product_id()).unwrap();
    executor
        .block_on(handle_fork(
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        ))
        .expect("the executor drives the call")
        .unwrap();
    assert_ne!(config.session.id, source_id);
    executor.block_on(opened.release()).unwrap().unwrap();
    config.session = target;
    let resumed = executor
        .block_on(super::super::startup::build_session_from_config(
            &mut config,
            &mut Vec::new(),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(config.session.id, request.successor_product_id());
    assert!(resumed.agent().is_some());
    executor.block_on(resumed.release()).unwrap().unwrap();
}

#[test]
fn interrupted_source_projection_is_pre_dispatch_for_fork_and_rebind() {
    for rebind in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
            plakous_host_protocol::views::ProviderSection {
                account: "codex".into(),
                label: "Codex".into(),
                ready: true,
                entries: vec![plakous_host_protocol::views::ModelEntry {
                    id: "offline-loopback-v2".into(),
                    tag: "offline".into(),
                    reasoning: true,
                    effort_options: vec!["medium".into()],
                    extras: Vec::new(),
                    unavailable_reason: None,
                }],
            },
        ]);
        let mut opened = executor
            .block_on(super::super::startup::build_session_from_config(
                &mut config,
                &mut Vec::new(),
            ))
            .unwrap()
            .unwrap();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();
        config.store.save(&mut config.session).unwrap();
        let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        super::super::projection::probe::after_next_write(config.session.id.clone(), move || {
            actions.send(UserAction::Interrupt).unwrap();
        });
        let state = RefCell::new(InterruptWatchState::default());
        let (events, _rx) = host_event_sink();
        let watch = InterruptWatch::new(&receiver, &state);
        let error = if rebind {
            executor
                .block_on(super::super::rebind::handle_rebind(
                    watch,
                    &mut config,
                    &mut opened,
                    &mut Vec::new(),
                    &events,
                    "codex",
                    "offline-loopback-v2",
                    Some("medium"),
                    None,
                    None,
                    None,
                    &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
                    None,
                ))
                .expect("the executor drives the call")
                .unwrap_err()
        } else {
            executor
                .block_on(handle_fork(
                    watch,
                    &mut config,
                    &mut opened,
                    &mut Vec::new(),
                    &events,
                    None,
                ))
                .expect("the executor drives the call")
                .unwrap_err()
        };
        assert!(
            error.message().contains("before its canonical dispatch"),
            "{error:?}"
        );
        assert!(opened.agent().is_some());
        assert_eq!(
            config
                .store
                .load_intents(&config.session.id)
                .unwrap()
                .workflow_requests,
            [] as [sessions::projector::WorkflowRequest; 0]
        );
        executor.block_on(opened.release()).unwrap().unwrap();
    }
}

#[test]
fn live_fork_refuses_without_withdrawing_another_pending_request() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let mut opened = executor
        .block_on(super::super::startup::build_session_from_config(
            &mut config,
            &mut Vec::new(),
        ))
        .unwrap()
        .unwrap();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    let request = sessions::projector::WorkflowRequest {
        operation_id: "plak:compact:pending-at-fork".into(),
        source_locator: opened.locator(),
        kind: sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual),
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .unwrap();
    let (events, rx) = host_event_sink();
    let (_actions, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let error = executor
        .block_on(handle_fork(
            InterruptWatch::new(&actions_rx, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        ))
        .expect("the executor drives the call")
        .unwrap_err();
    assert!(error.message().contains("unresolved product work"));
    assert!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_request(&request.operation_id)
            .is_some()
    );
    assert!(
        rx.try_iter()
            .all(|event| !matches!(event, HostEvent::Row(TranscriptRow::Notice(_))))
    );
}

#[test]
fn attached_no_effort_openrouter_fork_uses_real_host_workflow() {
    let fork_root = tempfile::tempdir().unwrap();
    let fork_events = run_attached_actions(
        no_effort_openrouter_injected_host(fork_root.path()),
        vec![UserAction::ForkSession {
            account: None,
            compact: false,
        }],
    );
    assert!(fork_events.iter().any(|event| matches!(
        event,
        HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("forked session")
    )));
    assert!(fork_events.iter().all(|event| !matches!(
        event,
        HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("effort")
    )));
}

#[test]
fn fork_then_compact_on_a_short_session_keeps_the_fork() {
    let root = tempfile::tempdir().unwrap();
    let events = run_attached_actions(
        no_effort_openrouter_injected_host(root.path()),
        vec![UserAction::ForkSession {
            account: None,
            compact: true,
        }],
    );
    assert!(events.iter().any(|event| matches!(
        event,
        HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("forked session")
    )));
    assert!(events.iter().any(|event| {
        let text = match event {
            HostEvent::Row(TranscriptRow::Notice(
                plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
            )) => note.as_str(),
            _ => return false,
        };
        text.contains("no eligible") || text.contains("the forked session is active")
    }));
    assert!(events.iter().all(|event| !matches!(
        event,
        HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.starts_with("fork:")
    )));
}

/// The canonical fork landed but its session could not attach: the fork
/// is published as its own resumable session, the workflow resolves, and
/// this session stays attached and promptable.
#[test]
fn a_fork_that_cannot_attach_is_saved_and_the_source_keeps_working() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let (mut config, injection) = fresh_injected_attached_host(root.path());
    let primaries = std::sync::Arc::new(AtomicUsize::new(0));
    let runtime = std::sync::Arc::clone(&injection.runtime);
    let counted = std::sync::Arc::clone(&primaries);
    config.runtime_factory = std::sync::Arc::new(crate::runtime::ScriptedRuntimeFactory {
        // Primary runtimes: the source, the fork's compilation, then the
        // fork's own session.
        runtime: std::sync::Arc::new(move |role| {
            if role == RuntimeRole::Primary && counted.fetch_add(1, Ordering::SeqCst) == 2 {
                return Err("injected: the fork target cannot attach".to_owned());
            }
            runtime(role)
        }),
        rebind_target: injection.rebind_target.clone(),
        observe: std::sync::Arc::new(|_, _| {}),
        child_runtime: None,
        profile_route: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let store = std::sync::Arc::clone(&config.store);
    let source_id = config.session.id.clone();
    let events = run_attached_actions(
        config,
        vec![
            UserAction::ForkSession {
                account: None,
                compact: false,
            },
            UserAction::Prompt("still here".into()),
        ],
    );
    let note = events
        .iter()
        .find_map(|event| match event {
            HostEvent::Row(TranscriptRow::Notice(
                plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
            )) if note.contains("could not attach") => Some(note.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no attach failure reported: {events:#?}"));
    assert!(note.contains("this session stays attached"), "{note}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PromptStarted(Some(_)))),
        "the source took the next prompt: {events:#?}"
    );

    let intents = store.load_intents(&source_id).unwrap();
    let request = intents.workflow_requests.first().unwrap();
    assert_eq!(request.kind, sessions::SessionWorkflowKind::Fork);
    let views = store.load_views(&source_id).unwrap().unwrap();
    assert!(
        views.state.workflows[&request.operation_id]
            .settled
            .is_some()
    );
    let target_id = note
        .split_whitespace()
        .skip_while(|word| *word != "session")
        .nth(1)
        .unwrap();
    let target = store.load(target_id).unwrap();
    assert_eq!(
        target.source_product_session_id.as_deref(),
        Some(source_id.as_str())
    );
}

/// A fork refused because the session's tools changed says so in `/fork`
/// terms; other refusals keep the canonical text.
#[test]
fn a_fork_refused_for_changed_tools_says_so() {
    assert_eq!(
        fork_refusal(
            AgentSessionsErrorKind::Revalidation,
            "fork changed canonical tool authority outside its authority",
        ),
        Some("cannot fork: this session's tools changed; start a new session")
    );
    assert_eq!(
        fork_refusal(
            AgentSessionsErrorKind::Revalidation,
            "fork changed effect policy outside its authority",
        ),
        None
    );
}

#[test]
fn interrupt_refuses_fork_before_any_durable_work() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let mut opened = memory_attached_stub();
    let source = opened.locator();
    config.session.bind_plakous_locator(source).unwrap();
    let (events, _rx) = host_event_sink();
    let (actions_tx, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    actions_tx.send(UserAction::Interrupt).unwrap();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);

    let error = ApplicationExecutor::new()
        .unwrap()
        .block_on(handle_fork(
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        ))
        .expect("the executor drives the call")
        .expect_err("an interrupted fork must not commit");

    assert!(
        error.message().contains("interrupted"),
        "fork refusal must name the interrupt: {}",
        error.message()
    );
    assert_eq!(config.session.plakous_locator, Some(source));
}
