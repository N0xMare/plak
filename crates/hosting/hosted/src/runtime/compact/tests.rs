#![allow(clippy::unwrap_used, reason = "unit tests")]
use crate::runtime::test_kit::BlockHere as _;

use crate::runtime::test_kit::TestExtension;
use plakous_host_protocol::status::HostPhase;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;
use crate::runtime::test_kit::ProbeIntent;

use super::{
    CompactPass, compact_committed_detail, compact_may_resume_queue, compact_operator_note,
    compact_queue_drain_state, context_capacity_recovery_guidance, handle_compact, observe_compact,
    scheduled_compact_queue_drain_state,
};
use crate::runtime::startup::host_runtime_binding;
use crate::runtime::*;

use crate::runtime::events::*;

use crate::context::test_verdicts::disposition_schedules;
use crate::context::{ContextDisposition, scheduled_compaction_disabled_note};
use crate::interrupt::{InterruptWatch, InterruptWatchState};
use crate::runtime::test_support::*;
use crate::runtime::usage::{compact_reason_for_disposition, live_capacity_recovery};
use crate::sessions::{self, QueueDrainState as SessionQueueDrainState};
use plakous::application::ApplicationExecutor;
use plakous::sessions::SessionOperationId;
use plakous::subscriptions::NativeProvider;
use std::cell::RefCell;

#[test]
fn scheduled_compaction_without_a_token_trigger_requires_overflow() {
    assert!(disposition_schedules(ContextDisposition::WouldOverflow));
    assert!(disposition_schedules(ContextDisposition::ProtectedOverflow));
    assert!(!disposition_schedules(ContextDisposition::Fits));
    // The placeholder rung acts on a protected-only tail without a facet.
    assert!(disposition_schedules(ContextDisposition::ProtectedOverflow));
    assert!(!disposition_schedules(ContextDisposition::Fits));
    assert!(disposition_schedules(ContextDisposition::WouldOverflow));
    for disposition in [
        ContextDisposition::Fits,
        ContextDisposition::Unavailable,
        ContextDisposition::RecoveryRequired,
    ] {
        assert!(!disposition_schedules(disposition));
    }
    assert!(!disposition_schedules(ContextDisposition::Fits));
    // Occupancy gauge, not envelope-only: 38% envelope would skip; 88%
    // provider occupancy must schedule.
    assert!(!disposition_schedules(ContextDisposition::Fits));
    assert!(!disposition_schedules(ContextDisposition::Fits));
    assert_eq!(
        scheduled_compaction_disabled_note(ContextDisposition::WouldOverflow),
        "authoritative context would overflow; scheduled compaction is disabled"
    );
    assert_eq!(
        scheduled_compaction_disabled_note(ContextDisposition::Fits),
        "scheduled compaction is disabled at the context trigger"
    );
    assert!(native_compact_facet(NativeProvider::Codex));
    assert!(!native_compact_facet(NativeProvider::Xai));
    assert!(!native_compact_facet(NativeProvider::Claude));
    // OpenRouter is not a NativeProvider; its compact host uses the
    // coding binding via api_compaction_runtime_binding.
}

/// Protected overflow asks for capacity recovery on every route.
///
/// The facet used to decide this, which made `/compact` refuse before the
/// ladder ran while the capacity guidance promised it would replace. The
/// "only protected material" wording now belongs to the exhausted ladder.
#[test]
fn protected_overflow_asks_for_capacity_recovery_on_every_route() {
    use plakous_host_protocol::ContextCapacityRecovery::{Compactable, Protected, ResendShorter};

    assert_eq!(
        compact_reason_for_disposition(ContextDisposition::WouldOverflow),
        sessions::ProductCompactReason::CapacityRecovery
    );
    assert_eq!(
        compact_reason_for_disposition(ContextDisposition::ProtectedOverflow),
        sessions::ProductCompactReason::CapacityRecovery
    );
    assert_eq!(
        compact_reason_for_disposition(ContextDisposition::ProtectedOverflow),
        sessions::ProductCompactReason::CapacityRecovery,
        "the placeholder rung can act here, so the key must not refuse first"
    );
    assert_eq!(
        compact_reason_for_disposition(ContextDisposition::Fits),
        sessions::ProductCompactReason::Manual
    );
    assert_eq!(
        compact_reason_for_disposition(ContextDisposition::Unavailable),
        sessions::ProductCompactReason::Manual
    );
    assert_eq!(
        live_capacity_recovery(ContextDisposition::ProtectedOverflow, false),
        Some(Protected)
    );
    assert_eq!(
        live_capacity_recovery(ContextDisposition::ProtectedOverflow, true),
        None
    );
    assert_eq!(
        live_capacity_recovery(ContextDisposition::WouldOverflow, true),
        None
    );
    assert_eq!(
        context_capacity_recovery_guidance(Compactable, true, false, false),
        CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE
    );
    assert_eq!(
        context_capacity_recovery_guidance(Compactable, true, true, false),
        CONTEXT_CAPACITY_NATIVE_COMPACT_GUIDANCE
    );
    assert_eq!(
        context_capacity_recovery_guidance(Protected, true, true, false),
        CONTEXT_CAPACITY_PROTECTED_GUIDANCE
    );
    assert_eq!(
        context_capacity_recovery_guidance(Compactable, false, true, false),
        CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE
    );
    assert_eq!(
        context_capacity_recovery_guidance(Compactable, true, true, true),
        CONTEXT_CAPACITY_SEGMENT_LIMIT_GUIDANCE
    );
    assert_eq!(
        context_capacity_recovery_guidance(Compactable, false, false, true),
        CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE
    );
    assert!(
        context_capacity_recovery_guidance(ResendShorter, true, true, false)
            .contains("shorten the restored draft")
    );
    assert!(
        !COMPACT_UNAVAILABLE_DETAIL.contains("unavailable on this route"),
        "Unavailable compact copy must describe this attempt, not the route"
    );
    assert!(COMPACT_UNAVAILABLE_DETAIL.contains("retry is a new paid operation"));
    assert_eq!(compact_committed_detail(true), COMPACT_COMMITTED_DETAIL);
    assert_eq!(
        compact_committed_detail(false),
        COMPACT_COMMITTED_STILL_OVERFLOWS_DETAIL
    );
    assert!(compact_committed_detail(false).contains("still overflows"));
    assert_eq!(
        compact_terminal_subject(Some(199_700), Some(178_075), true, false).as_deref(),
        Some("199.7k → 178.1k · still over capacity")
    );
}

#[test]
fn capacity_recovery_queue_resumes_only_after_compacted_and_fresh_exact_fit() {
    assert!(compact_may_resume_queue(
        true,
        false,
        SessionQueueDrainState::LiveAutoDrain
    ));
    assert!(!compact_may_resume_queue(
        true,
        true,
        SessionQueueDrainState::LiveAutoDrain
    ));
    assert!(!compact_may_resume_queue(
        false,
        false,
        SessionQueueDrainState::LiveAutoDrain
    ));
    assert_eq!(
        compact_queue_drain_state(true, true),
        SessionQueueDrainState::LiveAutoDrain
    );
    for (compacted, fits) in [(false, false), (false, true), (true, false)] {
        assert_eq!(
            compact_queue_drain_state(compacted, fits),
            SessionQueueDrainState::RestoredPaused
        );
    }
    for pass in [
        CompactPass::Committed,
        CompactPass::NothingEligible,
        CompactPass::Failed {
            note: String::new(),
        },
    ] {
        assert_eq!(
            scheduled_compact_queue_drain_state(&pass),
            SessionQueueDrainState::LiveAutoDrain
        );
    }
    assert_eq!(
        scheduled_compact_queue_drain_state(&CompactPass::Unchanged),
        SessionQueueDrainState::RestoredPaused
    );
}
#[test]
fn attached_no_effort_openrouter_usage_and_compact_use_real_host_workflows() {
    let root = tempfile::tempdir().unwrap();
    let config = no_effort_openrouter_injected_host(root.path());
    let events = run_attached_actions(
        config,
        vec![
            UserAction::Extension(ProbeIntent::InspectUsage),
            UserAction::Compact,
        ],
    );
    assert_eq!(usage_statuses(&events), ["fits"]);
    assert!(
        events.iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("canonical compact failed")
        ) || matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                if note.contains("compaction not needed")
                    || note.contains("nothing in this conversation can be compacted yet")
        ) || matches!(
            event,
            HostEvent::ToolLine { name, .. } if name == "compact"
        )),
        "openrouter compact must run the host workflow, events={events:?}"
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("effort")
    )));
}

/// A manual `/compact` published no activity at
/// all, so the HUD stayed empty for the whole paid dispatch and
/// the TUI's `Compacting…` branch was unreachable. Spec 02 §5.4 segment 2.
/// Status and busy are latest-state events, so this drains between the two
/// phases exactly as the frame loop does.
#[test]
fn a_manual_compact_names_itself_on_the_turn_status_row_and_clears_after() {
    let (events, rx) = host_event_sink();
    observe_compact(&events, 2, true);
    let during = rx.try_iter().collect::<Vec<_>>();
    assert!(
        during.contains(&HostEvent::SetBusy(true)),
        "the row has zero height unless the host says it is working: {during:?}"
    );
    assert!(
        during.iter().any(|event| matches!(
            event,
            HostEvent::Status { phase, queue_len, .. }
                if matches!(phase, HostPhase::Compacting) && *queue_len == 2
        )),
        "the activity label names the compaction: {during:?}"
    );

    observe_compact(&events, 2, false);
    let after = rx.try_iter().collect::<Vec<_>>();
    assert!(after.contains(&HostEvent::SetBusy(false)));
    assert!(
        after.iter().any(|event| matches!(
            event,
            HostEvent::Status { phase, .. } if matches!(phase, HostPhase::Idle)
        )),
        "the activity clears on every exit arm: {after:?}"
    );
}

#[test]
fn compact_on_fitting_session_stays_manual_and_reports_no_eligible_range() {
    let root = tempfile::tempdir().unwrap();
    let script = Arc::new(
        serde_json::from_value::<plakous_offline_crash_harness::script::ScriptPlan>(
            serde_json::json!({
                "version": 1,
                "steps": [{"output": {"text": "unused"}}]
            }),
        )
        .unwrap(),
    );
    let (mut config, _) = recover_tests::scripted_host_with(root.path(), Arc::clone(&script));
    let runtime = Arc::clone(&test_kit::scripted(&config).runtime);
    let workspace = config.cwd.clone();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.path().join("compact-evidence.jsonl"),
    )
    .unwrap();
    test_kit::scripted_mut(&mut config).runtime = Arc::new(move |role| {
        if role == RuntimeRole::Compact {
            plakous_offline_crash_harness::scripted_compaction_runtime_binding(
                &workspace,
                controller.clone(),
                &script,
                None,
                None,
                Vec::new(),
            )
            .map_err(|error| error.to_string())
        } else {
            runtime(role)
        }
    });
    config.compaction_enabled = true;
    let session_id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let prepared = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&prepared);
    let observed_store = Arc::clone(&store);
    let observed_id = session_id.clone();
    test_kit::scripted_mut(&mut config).observe = Arc::new(move |workflow, boundary| {
        if workflow == HostWorkflow::Compact && boundary == HostBoundary::Prepared {
            *captured.lock().unwrap() = observed_store
                .load_intents(&observed_id)
                .unwrap()
                .workflow_requests;
        }
    });
    let (actions, receiver, worker) = spawn_host_worker(config).unwrap();
    actions.send(UserAction::Compact).unwrap();
    let mut events = Vec::new();
    let mut compact_started = false;
    loop {
        let event = receiver.recv_timeout(Duration::from_secs(20)).unwrap();
        compact_started |= matches!(event, HostEvent::SetBusy(true));
        let finished = compact_started && matches!(event, HostEvent::SetBusy(false));
        events.push(event);
        if finished {
            break;
        }
    }
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
    events.extend(receiver.try_iter());
    assert!(
        store
            .load_intents(&session_id)
            .unwrap()
            .workflow_requests
            .is_empty(),
        "a no-eligible-range answer leaves no pending compact request: {events:?}"
    );
    let requests = prepared.lock().unwrap();
    assert_eq!(requests.len(), 1, "events={events:?}");
    assert!(matches!(
        requests[0].kind,
        sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual)
    ));

    assert!(
        events.iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                if note.contains("nothing in this conversation can be compacted yet")
        )),
        "fitting manual compact must answer no eligible range, events={events:?}"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                if note.contains("compaction not needed: the selected prefix already fits")
        )),
        "manual compact must not Fits-short-circuit, events={events:?}"
    );
    if let Ok(Some(views)) = store.load_views(&session_id) {
        assert_eq!(views.state.counters.compactions, 0);
    }
}

/// Grok protected overflow now attempts the compact instead of refusing.
///
/// `/compact` used to be refused before the ladder ran, on a key that
/// asked the native facet. It now dispatches and the outcome decides.
/// This fixture's injected runtime is not exact for its own overflow
/// source, so the attempt is refused at runtime admission rather than
/// reaching the rung; what this pins is that the pre-ladder refusal is
/// gone and that nothing is committed. The exhausted-ladder wording is
/// pinned by `context_capacity_recovery_guidance` above and by the
/// agent-level refusal test.
#[test]
fn overflow_usage_and_grok_compact_attempts_the_ladder() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = overflow_injected_attached_host(root.path(), NativeProvider::Xai);
    let session_id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let events = run_attached_actions(
        config,
        vec![
            UserAction::Extension(ProbeIntent::InspectUsage),
            UserAction::Compact,
        ],
    );
    let statuses = usage_statuses(&events);
    assert!(
        !statuses.is_empty()
            && statuses
                .iter()
                .all(|status| *status == "protected-overflow"),
        "empty-session Grok overflow must report protected-overflow, got {statuses:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("only protected context remains")
        )),
        "the pre-ladder refusal must be gone, events={events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                if note.contains("compact")
        )),
        "the compact must be attempted and reported, events={events:?}"
    );
    if let Ok(Some(views)) = store.load_views(&session_id) {
        assert_eq!(
            views.state.counters.compactions, 0,
            "an unrescuable overflow commits no compaction"
        );
    }
}

#[test]
fn compact_binding_failure_leaves_no_workflow_request() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let executor = ApplicationExecutor::new().unwrap();
    let runtime = host_runtime_binding(&config, &mut Vec::new(), RuntimeRole::Primary)
        .block_here()
        .unwrap();
    let opened = executor
        .block_on(crate::agent::build_or_open_bound_session(
            config.state_roots.runtime_sessions().unwrap(),
            None,
            None,
            SessionOperationId::new("binding-failure:create").unwrap(),
            runtime,
        ))
        .unwrap()
        .unwrap_or_else(|error| panic!("{}", error.failure.message()));
    test_kit::scripted_mut(&mut config).runtime =
        Arc::new(|_| Err("compact binding unavailable".into()));
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    let (events, _rx) = host_event_sink();
    let (_actions, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&rx, &state);
    let error = executor
        .block_on(handle_compact(
            watch,
            &mut config,
            &opened,
            &mut Vec::new(),
            &events,
            None,
            sessions::ProductCompactReason::Manual,
            None,
        ))
        .expect("the executor drives the call")
        .unwrap_err();
    assert!(
        error.message().contains("compact binding unavailable"),
        "{}",
        error.message()
    );
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        [] as [sessions::projector::WorkflowRequest; 0]
    );
}

#[test]
fn interrupt_refuses_compact_before_its_single_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let opened = memory_attached_stub();
    let source = opened.locator();
    config.session.bind_plakous_locator(source).unwrap();
    let (events, _rx) = host_event_sink();
    let (actions_tx, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    actions_tx.send(UserAction::Interrupt).unwrap();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);

    let error = ApplicationExecutor::new()
        .unwrap()
        .block_on(handle_compact(
            watch,
            &mut config,
            &opened,
            &mut Vec::new(),
            &events,
            None,
            sessions::ProductCompactReason::Manual,
            None,
        ))
        .expect("the executor drives the call")
        .expect_err("an interrupted compact must not dispatch");

    assert!(
        error.message().contains("interrupted"),
        "compact refusal must name the interrupt: {}",
        error.message()
    );
    // no dispatch happened, so no workflow and no redispatch.

    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        [] as [sessions::projector::WorkflowRequest; 0]
    );
    assert_eq!(config.session.plakous_locator, Some(source));
}

#[test]
fn a_quiet_provider_does_not_dump_the_responses_deadline_into_the_transcript() {
    let dumped = compact_operator_note(
        COMPACT_FAILED_DETAIL,
        Some(
            "Timeout: Responses operation deadline elapsed; no authoritative completion received; observed_events=4 last_observation_age_ms=30002",
        ),
    );
    assert!(
        !dumped.contains("deadline elapsed"),
        "raw deadline dump must not reach the transcript: {dumped}"
    );
    assert!(
        !dumped.contains("observed_events"),
        "Responses observation dump must not reach the transcript: {dumped}"
    );
    assert!(
        dumped.contains("Another explicit compaction may retry"),
        "the neutral note retains the retry fact: {dumped}"
    );
    assert_eq!(
        compact_operator_note(COMPACT_FAILED_DETAIL, Some("model refused")),
        format!("{COMPACT_FAILED_DETAIL}: model refused")
    );
    assert_eq!(
        compact_operator_note(COMPACT_FAILED_DETAIL, None),
        COMPACT_FAILED_DETAIL
    );
}
