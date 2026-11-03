#![allow(
    clippy::too_many_lines,
    reason = "Scheduled compaction scenarios preserve paid-dispatch identity and successor admission through real placeholder and capacity transitions."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]
use crate::runtime::test_kit::BlockHere as _;

use super::{
    SCHEDULED_COMPACT_FAILED_FITS_NOTE, SCHEDULED_COMPACT_MAIL_DEFERRED_NOTE,
    SCHEDULED_COMPACT_MAIL_UNREAD_CLAUSE, SCHEDULED_COMPACT_NOTHING_ELIGIBLE_NOTE, ScheduledPrompt,
    run_scheduled_compaction_until_fits_with, scheduled_compact_queue_drain_state,
    scheduled_paid_outcome, scheduled_prompt_sendable,
};
use crate::context::test_verdicts::idle_fence_schedules;
use crate::context::{ContextDisposition, ContextProjection, PreparedInputCount};
use crate::interrupt::{InterruptWatch, InterruptWatchState};
use crate::runtime::compact::{CompactPass, handle_compact};
use crate::runtime::startup::host_runtime_binding;
use crate::runtime::test_kit::{ScriptedRuntimeFactory, TestExtension};
use crate::runtime::test_support::{fresh_injected_attached_host, host_event_sink};
use crate::runtime::usage::last_enrolled_source_bytes;
use crate::runtime::{
    COMPACT_FAILED_DETAIL, RuntimeRole, projection, queue, recover_tests, test_kit,
};
use crate::sessions::{self, QueueDrainState as SessionQueueDrainState, TranscriptRow};
use plakous::agent::UserInput;
use plakous::application::ApplicationExecutor;
use plakous::sessions::SessionOperationId;
use std::cell::RefCell;
use std::sync::Arc;

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;

#[test]
fn scheduled_length_failure_uses_live_paid_flag_after_a_real_placeholder_commit() {
    use plakous::agent::{CommandId, TurnResultOutcome};
    use plakous::model::FinishReason;
    use plakous::sessions::ContextQuery;
    use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
    use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};

    let root = tempfile::tempdir().unwrap();
    let script: ScriptPlan = serde_json::from_value(serde_json::json!({
        "version": SCRIPT_VERSION,
        "maximum_projected_input": 100_000,
        "content_sensitive_projection": true,
        "tool_result_bytes": 20_000,
        "steps": [
            {"output": {"text": "x".repeat(60_000)}},
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": "latest protected answer"}}
        ]
    })).unwrap();
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
                plakous_offline_crash_harness::scripted_compaction_runtime_binding_with_finish(
                    &workspace,
                    controller.clone(),
                    &script,
                    None,
                    None,
                    Vec::new(),
                    FinishReason::Length,
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
    let runtime = host_runtime_binding(&config, &mut Vec::new(), RuntimeRole::Primary)
        .block_here()
        .unwrap();
    let opened = executor
        .block_on(crate::agent::build_or_open_bound_session(
            config.state_roots.runtime_sessions().unwrap(),
            None,
            None,
            SessionOperationId::new("scheduled-length:create").unwrap(),
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
                        CommandId::new(format!("scheduled-length:seed:{index}")).unwrap(),
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
    let source = opened.locator();
    let user_text = "p".repeat(50_000);
    let pending = UserInput::from(user_text.clone());
    queue::enqueue(&mut config.session.pending_queue, user_text).unwrap();
    config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
    let dispatches = || {
        read_evidence(&evidence)
            .unwrap()
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
            .count()
    };
    let before = dispatches();
    let (events, rx) = host_event_sink();
    let (_actions, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
    let admitted = executor
        .block_on(super::run_scheduled_compaction_until_fits(
            watch,
            &mut config,
            &opened,
            &mut Vec::new(),
            &events,
            None,
            &pending,
            None,
        ))
        .unwrap()
        .unwrap();
    assert_ne!(opened.locator(), source, "the unpaid successor must commit");
    assert!(opened.paid_fold_failed_since_last_round());
    assert_eq!(
        dispatches() - before,
        1,
        "no second paid dispatch at this fence"
    );
    let after = executor
        .block_on(crate::runtime::usage::query_context(
            &opened,
            ContextQuery::WithPendingUserInput(pending),
        ))
        .unwrap()
        .unwrap();
    assert!(after.compaction_verdict().is_due());
    assert!(
        after.compaction_verdict().observation_headroom()
            || after.disposition() != ContextDisposition::Fits
    );
    let failed_note = rx.try_iter().any(|event| {
        matches!(event,
            HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.text.contains("paid summary failed; offline compaction committed")
        )
    });
    assert!(
        failed_note || admitted == ScheduledPrompt::Parked,
        "the real scheduled pass must apply Failed send/park policy"
    );
    assert_eq!(
        config
            .store
            .load(&config.session.id)
            .unwrap()
            .compaction_count,
        1
    );
    executor.block_on(opened.release()).unwrap().unwrap();
}

#[test]
fn a_free_commit_after_a_paid_failure_never_pays_for_a_scheduled_follow_on() {
    let due = ContextProjection::test_fits(
        PreparedInputCount::UpperBound(99_000),
        100_000,
        100_000,
        None,
    )
    .with_anchored_occupied_tokens(99_000);
    assert!(due.compaction_verdict().is_due());
    for after in [due, due.with_disposition(ContextDisposition::WouldOverflow)] {
        let pass = scheduled_paid_outcome(CompactPass::Committed, true);
        let (outcome, ran, _, _) = run_scripted_scheduled_loop(vec![pass], after, None);
        assert_eq!(ran, 1);
        assert_eq!(
            outcome,
            if after.disposition() == ContextDisposition::Fits {
                ScheduledPrompt::Send {
                    over_window: false,
                    drop_mail: false,
                }
            } else {
                ScheduledPrompt::Parked
            }
        );
    }
    assert_eq!(
        scheduled_paid_outcome(CompactPass::Committed, false),
        CompactPass::Committed
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
            async |config| {
                passes += 1;
                config.session.queue_drain_state =
                    scheduled_compact_queue_drain_state(&CompactPass::NothingEligible);
                Ok(CompactPass::NothingEligible)
            },
            async |_| Ok(projection),
        )
        .block_here()
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
                .any(|text| text.contains("Retrying compaction is a new paid operation")),
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
            async |config| {
                passes += 1;
                let pass = CompactPass::Failed {
                    note: COMPACT_FAILED_DETAIL.to_owned(),
                };
                config.session.queue_drain_state = scheduled_compact_queue_drain_state(&pass);
                Ok(pass)
            },
            async |_| Ok(projection),
        )
        .block_here()
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
                .any(|text| text.contains(super::SCHEDULED_COMPACT_PARKED_NOTE)),
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
        async |config| {
            ran += 1;
            let pass = scripted.next().expect("an unscripted follow-on pass");
            // "Already fits" returns Unchanged before any dispatch, so it
            // leaves drain as it was.
            if pass != CompactPass::Unchanged {
                config.session.queue_drain_state = scheduled_compact_queue_drain_state(&pass);
            }
            Ok(pass)
        },
        async |is_bare| Ok(if is_bare { bare.unwrap() } else { with_mail }),
    )
    .block_here()
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
    let runtime = host_runtime_binding(&config, &mut Vec::new(), RuntimeRole::Primary)
        .block_here()
        .unwrap();
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
    let admitted = executor
        .block_on(run_scheduled_compaction_until_fits_with(
            watch,
            &mut config,
            &events,
            last_enrolled_source_bytes(&opened),
            false,
            async |config| {
                compacts += 1;
                assert_eq!(
                    compacts, 1,
                    "a fitting successor must not start a follow-on fold"
                );
                handle_compact(
                    watch,
                    config,
                    &opened,
                    &mut Vec::new(),
                    &events,
                    None,
                    sessions::ProductCompactReason::Scheduled,
                    Some(&UserInput::from(user_text.clone())),
                )
                .await
            },
            async |_| Ok(after),
        ))
        .unwrap()
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
