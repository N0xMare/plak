//! A root turn its host lost holds its continuation for the one Resume, and
//! the open finds that loss again after a crash at any step (spec 03 §14.1).

#![allow(clippy::unwrap_used, reason = "test-owned hosts and stores")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};

use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
use crate::runtime::recover_tests::{script, scripted_host_with, turn_completed, wait_for};
use crate::runtime::root_loss::{
    ROOT_LOSS_RESUME_TEXT, RootLossState, is_root_loss_command, record_root_consent,
    root_loss_state,
};
use crate::runtime::spawn_host_worker;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::send_attached_action;
use crate::session_store::SessionRecordStore;
use crate::sessions::projector::{RootInterruptCause, RootStopAnchor};

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

struct Lost {
    store: Arc<dyn SessionRecordStore>,
    state_roots: crate::agent::ProductStateRoots,
    id: String,
    evidence: PathBuf,
}

fn dispatches(evidence: &Path) -> usize {
    read_evidence(evidence)
        .unwrap()
        .iter()
        .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
        .count()
}

/// A root turn whose host exits by lease alone while its provider request
/// stalls past cancellation: the next exact open closes it as lost.
fn lose_a_root_turn(root: &Path) -> Lost {
    let plan: Arc<ScriptPlan> = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": SCRIPT_VERSION,
            "stall_ignores_cancellation": true,
            "steps": [{"stream": [{"text": "lost prefix"}, {"stall": 30000}]}]
        }))
        .unwrap(),
    );
    let (config, evidence) = scripted_host_with(root, plan);
    let id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let state_roots = config.state_roots.clone();
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(
        &events,
        &mut seen,
        "the lost turn's prefix",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("lost prefix")),
    );
    actions.request_abandon_and_release();
    let _ = host.join().unwrap();
    Lost {
        store,
        state_roots,
        id,
        evidence,
    }
}

/// The next host process for the lost session, answering with `steps`.
fn next_host(
    root: &Path,
    lost: &Lost,
    steps: serde_json::Value,
    store: Arc<dyn SessionRecordStore>,
) -> crate::runtime::HostConfig<TestExtension> {
    let (mut config, _) = scripted_host_with(root, script(steps));
    config.state_roots = lost.state_roots.clone();
    config.store = store;
    config.session = lost.store.load(&lost.id).unwrap();
    config
}

fn held_notice(event: &HostEvent) -> bool {
    matches!(event, HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(notice))
        if notice.text.contains("the turn the lost host was running is held"))
}

fn idle(event: &HostEvent) -> bool {
    matches!(event, HostEvent::Status { phase, .. }
        if matches!(phase, plakous_host_protocol::status::HostPhase::Idle))
}

/// Opens the next host, waits for it to settle idle, and quits it.
fn open_and_quit(config: crate::runtime::HostConfig<TestExtension>) -> Vec<HostEvent> {
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the open", idle);
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
    seen.extend(events.try_iter());
    seen
}

/// The open after a lost host records one owner-lost interrupt anchored on
/// the lost turn and continues nothing by itself; the one Resume records
/// consent and runs one continuation; a later open holds nothing and
/// records nothing again.
#[test]
fn a_lost_root_turn_holds_until_the_one_resume_continues_it_once() {
    let root = tempfile::tempdir().unwrap();
    let lost = lose_a_root_turn(root.path());
    assert_eq!(dispatches(&lost.evidence), 1);
    let config = next_host(
        root.path(),
        &lost,
        serde_json::json!([{"output": {"text": "continued after the loss"}}]),
        Arc::clone(&lost.store),
    );
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the held notice", held_notice);
    let intents = lost.store.load_intents(&lost.id).unwrap();
    assert_eq!(intents.root_interrupts.len(), 1, "{intents:#?}");
    assert_eq!(
        intents.root_interrupts[0].value.cause,
        RootInterruptCause::OwnerLost
    );
    assert!(matches!(
        intents.root_interrupts[0].value.anchor,
        RootStopAnchor::Turn { .. }
    ));
    assert!(
        intents.root_continuations.is_empty(),
        "nothing continues by itself"
    );
    assert_eq!(dispatches(&lost.evidence), 1);

    send_attached_action(
        &actions,
        UserAction::ResumeLossContinuation { agent_id: None },
    );
    wait_for(&events, &mut seen, "the continuation", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
    assert_eq!(dispatches(&lost.evidence), 2, "one continuation, no replay");
    let intents = lost.store.load_intents(&lost.id).unwrap();
    assert_eq!(intents.root_continuations.len(), 1);
    assert_eq!(
        intents
            .submissions
            .iter()
            .filter(|submission| is_root_loss_command(&submission.command_id))
            .count(),
        1
    );

    let seen = open_and_quit(next_host(
        root.path(),
        &lost,
        serde_json::json!([]),
        Arc::clone(&lost.store),
    ));
    assert!(!seen.iter().any(held_notice), "{seen:#?}");
    let intents = lost.store.load_intents(&lost.id).unwrap();
    assert_eq!(intents.root_interrupts.len(), 1, "recorded once");
    assert_eq!(intents.root_continuations.len(), 1);
    assert_eq!(dispatches(&lost.evidence), 2);
    let views = lost.store.load_views(&lost.id).unwrap().unwrap();
    assert!(
        !views.rows.values().any(|row| row
            .text()
            .is_some_and(|text| text.contains("the turn the lost host was running is held"))),
        "the held notice is live only"
    );
    assert!(
        !intents.notices.iter().any(|notice| notice
            .row
            .text()
            .is_some_and(|text| text.contains("the turn the lost host was running is held"))),
        "the held notice is never stored"
    );
}

struct RefuseRootLoss;
impl StoreFaults for RefuseRootLoss {
    fn before(
        &self,
        call: StoreCall,
        _session: Option<&crate::sessions::SessionRecord>,
    ) -> Result<(), String> {
        if call == StoreCall::WriteRootLoss {
            return Err("injected root loss refusal".to_owned());
        }
        Ok(())
    }
}

/// The window between the open's projection and the loss's write: an open
/// whose loss write fails has already applied the lost turn's terminal to
/// the views, and the next open still finds the loss in the feed.
#[test]
fn a_loss_left_unrecorded_after_the_projection_is_found_by_the_next_open() {
    let root = tempfile::tempdir().unwrap();
    let lost = lose_a_root_turn(root.path());
    let refusing: Arc<dyn SessionRecordStore> =
        Arc::new(FaultStore::new(Arc::clone(&lost.store), RefuseRootLoss));
    let seen = open_and_quit(next_host(
        root.path(),
        &lost,
        serde_json::json!([]),
        refusing,
    ));
    assert!(!seen.iter().any(held_notice), "{seen:#?}");
    assert_eq!(
        lost.store
            .load_intents(&lost.id)
            .unwrap()
            .root_interrupts
            .len(),
        0,
        "the refused write recorded nothing"
    );
    let views = lost.store.load_views(&lost.id).unwrap().unwrap();
    assert!(
        views.state.open_turns.is_empty(),
        "the projection applied the lost turn's terminal"
    );

    let seen = open_and_quit(next_host(
        root.path(),
        &lost,
        serde_json::json!([]),
        Arc::clone(&lost.store),
    ));
    assert!(seen.iter().any(held_notice), "{seen:#?}");
    let record = lost.store.load(&lost.id).unwrap();
    assert_eq!(
        root_loss_state(lost.store.as_ref(), &record).unwrap(),
        Some(RootLossState::Held)
    );
    assert_eq!(dispatches(&lost.evidence), 1);
}

/// Consent recorded, then a crash before its turn was prepared: the next
/// open runs the pending continuation once by itself, whatever the policy.
#[test]
fn a_consent_a_crash_left_unprepared_runs_once_at_the_next_open() {
    let root = tempfile::tempdir().unwrap();
    let lost = lose_a_root_turn(root.path());
    open_and_quit(next_host(
        root.path(),
        &lost,
        serde_json::json!([]),
        Arc::clone(&lost.store),
    ));
    let record = lost.store.load(&lost.id).unwrap();
    let command = record_root_consent(lost.store.as_ref(), &record)
        .unwrap()
        .unwrap();
    assert_eq!(
        root_loss_state(lost.store.as_ref(), &record).unwrap(),
        Some(RootLossState::Pending(command.clone()))
    );
    run_pending_continuation(root.path(), &lost, &command);
}

/// Consent and the prepared host-wake submission, then a crash before
/// dispatch: the next open keeps that queued item instead of discarding it
/// like an unadmitted wake, and runs it once under the same command id.
#[test]
fn a_continuation_a_crash_left_prepared_runs_once_at_the_next_open() {
    use crate::runtime::test_kit::BlockOn as _;
    let root = tempfile::tempdir().unwrap();
    let lost = lose_a_root_turn(root.path());
    open_and_quit(next_host(
        root.path(),
        &lost,
        serde_json::json!([]),
        Arc::clone(&lost.store),
    ));
    let record = lost.store.load(&lost.id).unwrap();
    let command = record_root_consent(lost.store.as_ref(), &record)
        .unwrap()
        .unwrap();
    let mut candidate = record.clone();
    let item = candidate.enqueue_live(ROOT_LOSS_RESUME_TEXT).unwrap();
    let rows = crate::runtime::HostRows::new(&lost.state_roots, lost.store.as_ref());
    crate::runtime::result_mail::prepare_with_command(
        &rows,
        lost.store.as_ref(),
        &candidate,
        &item,
        ROOT_LOSS_RESUME_TEXT,
        None,
        &command,
        crate::driver::ledger::CommandKind::HostWake,
    )
    .block()
    .unwrap();
    assert!(
        lost.store
            .load_intents(&lost.id)
            .unwrap()
            .submission(&command)
            .is_some()
    );
    run_pending_continuation(root.path(), &lost, &command);
    let intents = lost.store.load_intents(&lost.id).unwrap();
    assert_eq!(
        intents.submission(&command).unwrap().queue_item_id,
        item.id,
        "the queued item a crash left prepared ran, not a second one"
    );
}

/// Opens the next host on a pending continuation: it runs once, under
/// `command`, with no Resume; a later open runs nothing more.
fn run_pending_continuation(root: &Path, lost: &Lost, command: &str) {
    let config = next_host(
        root,
        lost,
        serde_json::json!([{"output": {"text": "continued after the crash"}}]),
        Arc::clone(&lost.store),
    );
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the continuation", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
    assert_eq!(dispatches(&lost.evidence), 2, "one continuation");
    let intents = lost.store.load_intents(&lost.id).unwrap();
    assert_eq!(intents.root_continuations.len(), 1);
    let continuations = intents
        .submissions
        .iter()
        .filter(|submission| is_root_loss_command(&submission.command_id))
        .map(|submission| submission.command_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(continuations, [command]);
    let record = lost.store.load(&lost.id).unwrap();
    assert_eq!(root_loss_state(lost.store.as_ref(), &record).unwrap(), None);

    open_and_quit(next_host(
        root,
        lost,
        serde_json::json!([]),
        Arc::clone(&lost.store),
    ));
    assert_eq!(dispatches(&lost.evidence), 2, "nothing more runs");
}

/// A turn a live owner fenced is not a loss of its owner: it ends
/// recovery-required, its in-place recovery and the session's next open
/// record no root interrupt, and nothing is held.
#[test]
fn a_fenced_root_turn_records_no_loss_and_holds_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (config, evidence) = scripted_host_with(
        root.path(),
        script(serde_json::json!([
            {"fault": {"at": "in_band", "kind": "outcome_unknown", "after_events": 2}},
            {"output": {"text": "the answer after recovery"}}
        ])),
    );
    let lost = Lost {
        store: Arc::clone(&config.store),
        state_roots: config.state_roots.clone(),
        id: config.session.id.clone(),
        evidence,
    };
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(&events, &mut seen, "the fenced terminal", |event| {
        matches!(event, HostEvent::TurnLifecycle(lifecycle)
            if format!("{lifecycle:?}").contains("RecoveryRequired"))
    });
    wait_for(&events, &mut seen, "the in-place recovery", |event| {
        matches!(event, HostEvent::Recovered { .. })
    });
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
    seen.extend(events.try_iter());
    assert!(!seen.iter().any(held_notice), "{seen:#?}");

    let reopened = open_and_quit(next_host(
        root.path(),
        &lost,
        serde_json::json!([]),
        Arc::clone(&lost.store),
    ));
    assert!(!reopened.iter().any(held_notice), "{reopened:#?}");
    let intents = lost.store.load_intents(&lost.id).unwrap();
    assert_eq!(intents.root_interrupts.len(), 0, "{intents:#?}");
    let record = lost.store.load(&lost.id).unwrap();
    assert_eq!(root_loss_state(lost.store.as_ref(), &record).unwrap(), None);
    assert_eq!(dispatches(&lost.evidence), 1, "nothing continued");
}
