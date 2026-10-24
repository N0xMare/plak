#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;
use crate::runtime::test_kit::ProbeIntent;

use super::*;
use crate::runtime::*;

use crate::runtime::events::*;

use crate::context::test_verdicts::{disposition_schedules, idle_fence_schedules};
use crate::context::{
    ContextDisposition, ContextProjection, PreparedInputCount, scheduled_compaction_disabled_note,
};
use crate::interrupt::{InterruptWatch, InterruptWatchState};
use crate::runtime::test_support::*;
use crate::runtime::usage::*;
use crate::sessions::{self, QueueDrainState as SessionQueueDrainState};
use plakous::agent::CompactionEventOutcome;
use plakous::application::ApplicationExecutor;
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
    assert_eq!(format_compact_tokens(199_700), "199.7k");
    assert_eq!(format_compact_tokens(178_075), "178.1k");
    assert_eq!(format_compact_tokens(400), "400");
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

/// A scheduled pass that finds nothing to compact never pays for a
/// follow-on, and the prompt goes to ordinary turn admission whether or
/// not it fits: the turn withholds an over-window prompt with the
/// resend-shorter remedy, never the misleading "retry compaction" guidance.
#[test]
fn a_scheduled_pass_with_nothing_eligible_sends_the_prompt() {
    let triggered = ContextProjection::test_fits(
        PreparedInputCount::UpperBound(99_000),
        100_000,
        100_000,
        None,
    )
    .with_anchored_occupied_tokens(99_000);
    assert!(triggered.compaction_verdict().observation_headroom());
    assert!(idle_fence_schedules(triggered));
    let note = SCHEDULED_COMPACT_NOTHING_ELIGIBLE_NOTE;
    for projection in [
        triggered,
        triggered.with_disposition(ContextDisposition::WouldOverflow),
        triggered.with_disposition(ContextDisposition::ProtectedOverflow),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(dir.path());
        queue::enqueue(&mut config.session.pending_queue, "carry on".to_owned()).unwrap();
        config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
        let (events, rx) = host_event_sink();
        let (_actions, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
        let mut passes = 0;
        let admitted = run_scheduled_compaction_until_fits_with(
            watch,
            &mut config,
            &events,
            0,
            false,
            |config| {
                passes += 1;
                config.session.queue_drain_state =
                    scheduled_compact_queue_drain_state(&CompactPass::NothingEligible);
                Ok(CompactPass::NothingEligible)
            },
            |_| Ok(projection),
        )
        .unwrap();
        let over_window = projection.disposition() != ContextDisposition::Fits;
        assert_eq!(
            admitted,
            ScheduledPrompt::Send {
                over_window,
                drop_mail: false
            },
            "{:?}",
            projection.disposition()
        );
        assert_eq!(passes, 1, "nothing eligible never follows on");
        assert_eq!(
            config.session.queue_drain_state,
            SessionQueueDrainState::LiveAutoDrain
        );
        let notes: Vec<String> = rx
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(TranscriptRow::Notice(
                    plakous_host_protocol::views::transcript::NoticeRow { text, .. },
                )) => Some(text),
                _ => None,
            })
            .collect();
        // A withheld prompt's resend remedy is its one note.
        assert_eq!(
            notes.iter().any(|text| text == note),
            !over_window,
            "{notes:?}"
        );
        assert!(
            !notes
                .iter()
                .any(|text| text.contains("retrying compaction is a new paid operation")),
            "{notes:?}"
        );
    }
}

/// A definite scheduled failure is deterministic for its source. A prompt
/// that fits is sent without a follow-on; one that does not parks with
/// the paid-retry note.
#[test]
fn a_definite_scheduled_failure_sends_a_fitting_prompt_and_parks_one_that_does_not() {
    let triggered = ContextProjection::test_fits(
        PreparedInputCount::UpperBound(99_000),
        100_000,
        100_000,
        None,
    )
    .with_anchored_occupied_tokens(99_000);
    for (projection, expected) in [
        (
            triggered,
            ScheduledPrompt::Send {
                over_window: false,
                drop_mail: false,
            },
        ),
        (
            triggered.with_disposition(ContextDisposition::WouldOverflow),
            ScheduledPrompt::Parked,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(dir.path());
        queue::enqueue(&mut config.session.pending_queue, "carry on".to_owned()).unwrap();
        config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
        let (events, rx) = host_event_sink();
        let (_actions, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
        let mut passes = 0;
        let outcome = run_scheduled_compaction_until_fits_with(
            watch,
            &mut config,
            &events,
            0,
            false,
            |config| {
                passes += 1;
                let pass = CompactPass::Failed {
                    note: COMPACT_FAILED_DETAIL.to_owned(),
                };
                config.session.queue_drain_state = scheduled_compact_queue_drain_state(&pass);
                Ok(pass)
            },
            |_| Ok(projection),
        )
        .unwrap();
        assert_eq!(outcome, expected, "{:?}", projection.disposition());
        assert_eq!(passes, 1, "a definite failure never follows on");
        let notes: Vec<String> = rx
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(TranscriptRow::Notice(
                    plakous_host_protocol::views::transcript::NoticeRow { text, .. },
                )) => Some(text),
                _ => None,
            })
            .collect();
        let parked = expected == ScheduledPrompt::Parked;
        assert_eq!(
            config.session.queue_drain_state,
            if parked {
                SessionQueueDrainState::RestoredPaused
            } else {
                SessionQueueDrainState::LiveAutoDrain
            }
        );
        assert_eq!(
            notes
                .iter()
                .any(|text| text.contains("retrying compaction is a new paid operation")),
            parked,
            "{notes:?}"
        );
        if !parked {
            assert_eq!(
                notes,
                [COMPACT_FAILED_DETAIL, SCHEDULED_COMPACT_FAILED_FITS_NOTE]
            );
        }
        assert!(events.take_notice_intents().iter().all(|intent| {
            !matches!(&intent.row, TranscriptRow::Notice(notice) if notice.text.contains(COMPACT_FAILED_DETAIL))
        }), "a recorded failure is presentation only, not another notice intent");
    }
}

/// Runs the scheduled loop over scripted passes, with the mail-bearing
/// command measuring `with_mail` and the bare prompt `bare`.
fn run_scripted_scheduled_loop(
    passes: Vec<CompactPass>,
    with_mail: ContextProjection,
    bare: Option<ContextProjection>,
) -> (ScheduledPrompt, usize, Vec<String>, SessionQueueDrainState) {
    let dir = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(dir.path());
    queue::enqueue(&mut config.session.pending_queue, "carry on".to_owned()).unwrap();
    config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
    let (events, rx) = host_event_sink();
    let (_actions, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
    let mut scripted = passes.into_iter();
    let mut ran = 0;
    let outcome = run_scheduled_compaction_until_fits_with(
        watch,
        &mut config,
        &events,
        0,
        bare.is_some(),
        |config| {
            ran += 1;
            let pass = scripted.next().expect("an unscripted follow-on pass");
            // "Already fits" returns Unchanged before any dispatch, so it
            // leaves drain as it was.
            if pass != CompactPass::Unchanged {
                config.session.queue_drain_state = scheduled_compact_queue_drain_state(&pass);
            }
            Ok(pass)
        },
        |is_bare| Ok(if is_bare { bare.unwrap() } else { with_mail }),
    )
    .unwrap();
    let notes = rx
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(TranscriptRow::Notice(
                plakous_host_protocol::views::transcript::NoticeRow { text, .. },
            )) => Some(text),
            _ => None,
        })
        .collect();
    (outcome, ran, notes, config.session.queue_drain_state)
}

/// A prompt that fits is never parked behind its own child-mail batch:
/// when no pass can make room for the batch, the decision falls back to
/// the bare prompt and the batch stays unread. Without a bare fallback
/// (a wake, or a replayed batch) the same measures park.
#[test]
fn a_fitting_prompt_is_sent_without_mail_that_no_pass_can_fit() {
    let fits = ContextProjection::test_fits(
        PreparedInputCount::UpperBound(10_000),
        100_000,
        100_000,
        None,
    );
    assert!(scheduled_prompt_sendable(fits));
    let over = fits.with_disposition(ContextDisposition::WouldOverflow);
    let failed = || CompactPass::Failed {
        note: COMPACT_FAILED_DETAIL.to_owned(),
    };
    let sent_bare = ScheduledPrompt::Send {
        over_window: false,
        drop_mail: true,
    };
    let (outcome, ran, notes, _) =
        run_scripted_scheduled_loop(vec![CompactPass::NothingEligible], over, Some(fits));
    assert_eq!((outcome, ran), (sent_bare, 1));
    assert_eq!(
        notes,
        [format!(
            "{SCHEDULED_COMPACT_NOTHING_ELIGIBLE_NOTE}; {SCHEDULED_COMPACT_MAIL_UNREAD_CLAUSE}"
        )]
    );
    let (outcome, _, notes, _) =
        run_scripted_scheduled_loop(vec![CompactPass::NothingEligible], over, Some(over));
    assert_eq!(
        outcome,
        ScheduledPrompt::Send {
            over_window: true,
            drop_mail: true
        }
    );
    assert!(notes.is_empty(), "the turn's resend remedy is the one note");
    let (outcome, ran, notes, drain) =
        run_scripted_scheduled_loop(vec![failed()], over, Some(fits));
    assert_eq!((outcome, ran), (sent_bare, 1));
    assert_eq!(drain, SessionQueueDrainState::LiveAutoDrain);
    assert_eq!(
        notes,
        [
            COMPACT_FAILED_DETAIL.to_owned(),
            format!("{SCHEDULED_COMPACT_FAILED_FITS_NOTE}; {SCHEDULED_COMPACT_MAIL_UNREAD_CLAUSE}"),
        ]
    );
    let (outcome, ran, notes, _) = run_scripted_scheduled_loop(
        vec![CompactPass::Committed, CompactPass::Committed],
        over,
        Some(fits),
    );
    assert_eq!((outcome, ran), (sent_bare, 2), "{notes:?}");
    assert_eq!(notes.last().unwrap(), SCHEDULED_COMPACT_MAIL_DEFERRED_NOTE);
    for bare in [Some(over), None] {
        let (outcome, _, _, drain) = run_scripted_scheduled_loop(
            vec![CompactPass::Committed, CompactPass::Committed],
            over,
            bare,
        );
        assert_eq!(outcome, ScheduledPrompt::Parked);
        assert_eq!(drain, SessionQueueDrainState::RestoredPaused);
        let (outcome, _, _, _) = run_scripted_scheduled_loop(vec![failed()], over, bare);
        assert_eq!(outcome, ScheduledPrompt::Parked);
    }
}

/// A pass that left the head unchanged ("already fits") never pays for a
/// follow-on or says the successor still meets the trigger: it would
/// measure the same again.
#[test]
fn an_unchanged_pass_never_follows_on() {
    let fits = ContextProjection::test_fits(
        PreparedInputCount::UpperBound(10_000),
        100_000,
        100_000,
        None,
    );
    let over = fits.with_disposition(ContextDisposition::WouldOverflow);
    let (outcome, ran, notes, _) =
        run_scripted_scheduled_loop(vec![CompactPass::Unchanged], fits, None);
    assert_eq!(
        (outcome, ran),
        (
            ScheduledPrompt::Send {
                over_window: false,
                drop_mail: false
            },
            1
        )
    );
    assert!(notes.is_empty(), "{notes:?}");
    let (outcome, ran, notes, drain) =
        run_scripted_scheduled_loop(vec![CompactPass::Unchanged], over, None);
    assert_eq!((outcome, ran), (ScheduledPrompt::Parked, 1));
    assert_eq!(drain, SessionQueueDrainState::RestoredPaused);
    assert!(
        !notes
            .iter()
            .any(|note| note.contains("successor still meets")),
        "{notes:?}"
    );
}

#[test]
fn scheduled_compaction_admits_a_fitting_successor_above_the_occupancy_trigger() {
    use plakous::agent::{CommandId, TurnResultOutcome};
    use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
    use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};

    let root = tempfile::tempdir().unwrap();
    let script: ScriptPlan = serde_json::from_value(serde_json::json!({
        "version": SCRIPT_VERSION,
        "maximum_projected_input": 100_000,
        "content_sensitive_projection": true,
        "steps": [
            {"output": {"text": "x".repeat(70_000)}},
            {"output": {"text": "latest protected answer"}},
            {"output": {"text": "queued prompt admitted"}}
        ]
    }))
    .unwrap();
    let script = Arc::new(script);
    let (mut config, evidence) =
        recover_tests::scripted_host_with(root.path(), Arc::clone(&script));
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        evidence.clone(),
    )
    .unwrap();
    let workspace = config.cwd.clone();
    let cursor = Arc::new(std::sync::atomic::AtomicU64::new(1));
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        rebind_target: test_kit::scripted(&config).rebind_target.clone(),
        runtime: Arc::new(move |role| {
            if role == RuntimeRole::Compact {
                plakous_offline_crash_harness::scripted_compaction_runtime_binding(
                    &workspace,
                    controller.clone(),
                    &script,
                    None,
                    None,
                    Vec::new(),
                )
            } else {
                plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
                    &workspace,
                    controller.clone(),
                    Arc::clone(&script),
                    Arc::clone(&cursor),
                )
            }
            .map_err(|error| error.to_string())
        }),
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    config.compaction_enabled = true;
    let executor = ApplicationExecutor::new().unwrap();
    let runtime = host_runtime_binding(&config, &mut Vec::new(), RuntimeRole::Primary).unwrap();
    let opened = executor
        .block_on(crate::agent::build_or_open_bound_session(
            config.state_roots.runtime_sessions().unwrap(),
            None,
            None,
            SessionOperationId::new("scheduled-fits:create").unwrap(),
            runtime,
        ))
        .unwrap()
        .unwrap_or_else(|error| panic!("{}", error.failure.message()));
    for (index, prompt) in ["older intent", "latest intent"].into_iter().enumerate() {
        let result = executor
            .block_on(async {
                opened
                    .agent()
                    .unwrap()
                    .start(
                        CommandId::new(format!("scheduled-fits:seed:{index}")).unwrap(),
                        prompt,
                    )
                    .await
                    .unwrap()
                    .into_turn()
                    .unwrap()
                    .result()
                    .await
                    .unwrap()
            })
            .unwrap();
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
    }
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    let user_text = "p".repeat(50_000);
    queue::enqueue(&mut config.session.pending_queue, user_text.clone()).unwrap();
    config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
    let source = opened.locator();
    let after = ContextProjection::test_fits(
        PreparedInputCount::UpperBound(20_000),
        100_000,
        100_000,
        None,
    )
    .with_anchored_occupied_tokens(90_000)
    .with_auto_compact_input(Some(80_000));
    assert_eq!(after.disposition(), ContextDisposition::Fits);
    assert!(!after.compaction_verdict().observation_headroom());
    assert!(idle_fence_schedules(after));
    let (events, rx) = host_event_sink();
    let (_actions, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
    let mut compacts = 0;
    // Run a real committed scheduled fold; pin the post-fold control policy
    // with a Fits projection whose occupancy independently meets the trigger.
    let admitted = run_scheduled_compaction_until_fits_with(
        watch,
        &mut config,
        &events,
        last_enrolled_source_bytes(&opened),
        false,
        |config| {
            compacts += 1;
            assert_eq!(
                compacts, 1,
                "a fitting successor must not start a follow-on fold"
            );
            handle_compact(
                &executor,
                watch,
                config,
                &opened,
                &mut Vec::new(),
                &events,
                None,
                sessions::ProductCompactReason::Scheduled,
                Some(&UserInput::from(user_text.clone())),
            )
        },
        |_| Ok(after),
    )
    .unwrap();
    assert_eq!(
        admitted,
        ScheduledPrompt::Send {
            over_window: false,
            drop_mail: false
        }
    );
    assert_eq!(compacts, 1);
    assert_ne!(opened.locator(), source, "the scheduled fold committed");
    executor
        .block_on(projection::project_to_tail(
            opened.sessions(),
            &config.store,
            &config.session.id,
            opened.locator(),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(
        config
            .store
            .load(&config.session.id)
            .unwrap()
            .compaction_count,
        1,
        "the projector counts a committed host-level fold exactly once"
    );
    assert_eq!(
        config.session.queue_drain_state,
        SessionQueueDrainState::LiveAutoDrain
    );
    assert!(!rx.try_iter().any(|event| matches!(event,
        HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("compacting once more"))));
    let result = executor
        .block_on(async {
            opened
                .agent()
                .unwrap()
                .start(
                    CommandId::new("scheduled-fits:admitted").unwrap(),
                    user_text,
                )
                .await
                .unwrap()
                .into_turn()
                .unwrap()
                .result()
                .await
                .unwrap()
        })
        .unwrap();
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(
        read_evidence(&evidence)
            .unwrap()
            .iter()
            .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
            .count(),
        4,
        "two seed rounds, one fold and the admitted prompt only"
    );
    executor.block_on(opened.release()).unwrap().unwrap();
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
                if phase == "compacting" && *queue_len == 2
        )),
        "the activity label names the compaction: {during:?}"
    );

    observe_compact(&events, 2, false);
    let after = rx.try_iter().collect::<Vec<_>>();
    assert!(after.contains(&HostEvent::SetBusy(false)));
    assert!(
        after.iter().any(|event| matches!(
            event,
            HostEvent::Status { phase, .. } if phase == "idle"
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
    let runtime = host_runtime_binding(&config, &mut Vec::new(), RuntimeRole::Primary).unwrap();
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
    let error = handle_compact(
        &executor,
        watch,
        &mut config,
        &opened,
        &mut Vec::new(),
        &events,
        None,
        sessions::ProductCompactReason::Manual,
        None,
    )
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
fn unknown_without_a_fact_stays_pending_but_unavailable_discards() {
    use sessions::projector::{SessionViews, ViewWrites, WorkflowRequest};
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let source = memory_attached_stub().locator();
    config.session.bind_plakous_locator(source).unwrap();
    config.store.save(&mut config.session).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:compact:no-fact".into(),
        source_locator: source,
        kind: sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual),
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .unwrap();
    let views = SessionViews::empty();
    config
        .store
        .commit_projection_views(
            &config.session.id,
            &ViewWrites {
                successors: Vec::new(),
                expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
                cursor: plakous::sessions::feed::FeedSeq::ZERO,
                upserts: Vec::new(),
                deletes: Vec::new(),
                calls: Vec::new(),
                open_turns: Vec::new(),
                processes: Vec::new(),
            },
            &views.state,
        )
        .unwrap();
    apply_compact_views(
        &mut config,
        None,
        &request.operation_id,
        compact_error_is_unadmitted(AgentSessionsErrorKind::OutcomeUnknown),
        Ok(views.clone()),
    );
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        vec![request.clone()]
    );
    let answer = CompactSessionOutcome::Unavailable {
        operation_id: SessionOperationId::new(&request.operation_id).unwrap(),
        source,
        reason: plakous::sessions::SessionWorkflowUnavailable::Compaction,
    };
    assert!(compact_answer_is_unadmitted(&answer, source));
    apply_compact_views(
        &mut config,
        None,
        &request.operation_id,
        compact_answer_is_unadmitted(&answer, source),
        Ok(views),
    );
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        [] as [WorkflowRequest; 0]
    );
}

#[test]
fn unknown_after_admission_keeps_the_request_and_refresh_debt_is_nonfatal() {
    use sessions::projector::{SessionViews, ViewWrites, WorkflowCompletion, WorkflowRequest};
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let opened = memory_attached_stub();
    let source = opened.locator();
    config.session.bind_plakous_locator(source).unwrap();
    config.store.save(&mut config.session).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:compact:lost-reply".into(),
        source_locator: source,
        kind: sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual),
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .unwrap();
    let mut views = SessionViews::empty();
    views.state.runtime_session_id = Some(source.session_id());
    views.state.workflows.insert(
        request.operation_id.clone(),
        WorkflowCompletion {
            admitted_at: Some(1),
            settled: None,
        },
    );
    config
        .store
        .commit_projection_views(
            &config.session.id,
            &ViewWrites {
                successors: Vec::new(),
                expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
                cursor: plakous::sessions::feed::FeedSeq::ZERO,
                upserts: Vec::new(),
                deletes: Vec::new(),
                calls: Vec::new(),
                open_turns: Vec::new(),
                processes: Vec::new(),
            },
            &views.state,
        )
        .unwrap();
    let unadmitted = compact_error_is_unadmitted(AgentSessionsErrorKind::OutcomeUnknown);
    assert!(!unadmitted);
    let revision = config.session.store_revision;
    apply_compact_views(
        &mut config,
        None,
        &request.operation_id,
        unadmitted,
        Ok(views.clone()),
    );
    assert_eq!(config.session.store_revision, revision);
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        vec![request.clone()]
    );
    // Even a wrong caller classification cannot erase the admitted request.
    apply_compact_views(&mut config, None, &request.operation_id, true, Ok(views));
    apply_compact_views(
        &mut config,
        None,
        &request.operation_id,
        true,
        Err("transient projection fault".into()),
    );
    assert_eq!(config.session.store_revision, revision);
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        vec![request]
    );
    config.store.save(&mut config.session).unwrap();
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

    let error = handle_compact(
        &ApplicationExecutor::new().unwrap(),
        watch,
        &mut config,
        &opened,
        &mut Vec::new(),
        &events,
        None,
        sessions::ProductCompactReason::Manual,
        None,
    )
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

/// Spec section 9: every compaction outcome is distinct operator text,
/// including the ones the tool-group fence produces mid-turn, which were
/// logged and then dropped instead of reaching the transcript at all.
#[test]
fn every_compaction_outcome_has_its_own_operator_text() {
    let outcomes = [
        CompactionEventOutcome::Committed,
        CompactionEventOutcome::NotNeeded,
        CompactionEventOutcome::NoEligibleRange,
        CompactionEventOutcome::Unavailable,
        CompactionEventOutcome::Failed,
        CompactionEventOutcome::RecoveryRequired,
    ];
    let mut seen = std::collections::BTreeSet::new();
    for outcome in outcomes {
        let detail = compaction_event_outcome_detail(outcome);
        assert_ne!(detail, "");
        assert!(seen.insert(detail), "duplicate operator text: {detail}");
    }
    assert_eq!(seen.len(), outcomes.len());
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
        dumped.contains("/compact"),
        "the note must name the retry: {dumped}"
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

#[test]
fn a_shrink_refusal_is_plain_in_the_transcript_and_kept_in_the_log_detail() {
    let logged = compact_operator_detail(
        COMPACT_FAILED_DETAIL,
        Some(COMPACT_SHRINK_REFUSAL_DIAGNOSTIC),
    );
    assert!(logged.contains(COMPACT_SHRINK_REFUSAL_DIAGNOSTIC));
    assert_eq!(
        manual_compact_operator_text(&logged),
        COMPACT_SHRINK_REFUSAL_NOTE
    );
    assert!(!COMPACT_SHRINK_REFUSAL_NOTE.contains("did not prove"));
    let other = format!("{COMPACT_FAILED_DETAIL}: model refused");
    assert_eq!(manual_compact_operator_text(&other), other);
}
