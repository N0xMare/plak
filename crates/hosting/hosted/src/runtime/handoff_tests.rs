//! A planned handoff, driven at the host by `request_handoff_and_release`
//! (spec 03 §14.1): the releasing host records `interrupt{handoff}` for each
//! node with work in flight, durable before any lease is released, and the
//! next host continues each once by itself, `max_turns` applied; a later
//! open continues nothing again.

#![allow(clippy::unwrap_used, reason = "test-owned hosts and stores")]

use std::sync::{Arc, Mutex};

use crate::agent_tree::ContinuationCause;
use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
use crate::runtime::recover_tests::{script, scripted_host_with, turn_completed, wait_for};
use crate::runtime::root_loss::{RootLossState, root_loss_state};
use crate::runtime::root_loss_open_tests::{
    Release, dispatches, held_notice, idle, next_host, open_and_quit, release_a_running_root_turn,
};
use crate::runtime::spawn_host_worker;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::send_attached_action;
use crate::sessions::projector::{RootContinuationCause, RootInterruptCause};

type UserAction = crate::surface_protocol::UserAction<TestExtension>;

/// The root's interrupt causes the store holds when the handoff is recorded.
fn record_root_causes(
    config: crate::runtime::HostConfig<TestExtension>,
    seen: &Arc<Mutex<Option<Vec<RootInterruptCause>>>>,
) -> crate::runtime::HostConfig<TestExtension> {
    let (store, id, seen) = (
        Arc::clone(&config.store),
        config.session.id.clone(),
        Arc::clone(seen),
    );
    super::host_loop::handoff_hooks::after_record(move || {
        let causes = store
            .load_intents(&id)
            .unwrap()
            .root_interrupts
            .iter()
            .map(|intent| intent.value.cause)
            .collect();
        *seen.lock().unwrap() = Some(causes);
    });
    config
}

/// A running root turn handed off, whether its cancel settled or the
/// release abandoned it by lease: the handoff is durable before the lease
/// release, the next open continues the turn once by itself under hold, and
/// a later open continues nothing.
#[test]
fn a_handed_off_root_turn_continues_once_by_itself() {
    for deaf in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(None));
        let lost = release_a_running_root_turn(root.path(), Release::Handoff, deaf, |config| {
            record_root_causes(config, &seen)
        });
        assert_eq!(
            seen.lock().unwrap().clone(),
            Some(vec![RootInterruptCause::Handoff]),
            "recorded before the release (deaf={deaf})"
        );
        assert_eq!(dispatches(&lost.evidence), 1);

        let config = next_host(
            root.path(),
            &lost,
            serde_json::json!([{"output": {"text": "continued after the handoff"}}]),
            Arc::clone(&lost.store),
        );
        let (actions, events, host) = spawn_host_worker(config).unwrap();
        let mut events_seen = Vec::new();
        wait_for(
            &events,
            &mut events_seen,
            "the continuation",
            turn_completed,
        );
        send_attached_action(&actions, UserAction::Quit);
        host.join().unwrap().unwrap();
        assert!(!events_seen.iter().any(held_notice), "deaf={deaf}");
        assert_eq!(
            dispatches(&lost.evidence),
            2,
            "one continuation (deaf={deaf})"
        );
        let intents = lost.store.load_intents(&lost.id).unwrap();
        assert_eq!(
            intents
                .root_interrupts
                .iter()
                .map(|intent| intent.value.cause)
                .collect::<Vec<_>>(),
            [RootInterruptCause::Handoff],
            "the loss of the handed-off turn is not recorded again (deaf={deaf})"
        );
        assert_eq!(intents.root_continuations.len(), 1);
        assert_eq!(
            intents.root_continuations[0].value.cause,
            RootContinuationCause::Handoff
        );

        open_and_quit(next_host(
            root.path(),
            &lost,
            serde_json::json!([]),
            Arc::clone(&lost.store),
        ));
        assert_eq!(dispatches(&lost.evidence), 2, "nothing more (deaf={deaf})");
        assert_eq!(
            lost.store
                .load_intents(&lost.id)
                .unwrap()
                .root_continuations
                .len(),
            1
        );
    }
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

/// A handoff whose write fails still releases, without the record: the
/// next open reads an ordinary loss and holds, and nothing continues by
/// itself without a durable handoff row.
#[test]
fn a_handoff_the_host_could_not_record_holds_as_a_loss() {
    let root = tempfile::tempdir().unwrap();
    let lost = release_a_running_root_turn(root.path(), Release::Handoff, true, |mut config| {
        config.store = Arc::new(FaultStore::new(Arc::clone(&config.store), RefuseRootLoss));
        config
    });
    assert_eq!(
        lost.store
            .load_intents(&lost.id)
            .unwrap()
            .root_interrupts
            .len(),
        0,
        "the refused write recorded nothing"
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
    assert_eq!(dispatches(&lost.evidence), 1, "nothing continued");
}

/// The tree's half, through the host: a parked child is handed off before
/// the release, and a parked child whose attempt spent its `max_turns` is
/// interrupted but owed no continuation. A child whose rows still say
/// running while its runtime turn already ended recovery-required is not
/// handed off: the releasing host projects its feed first, which settles
/// it, and its recovery takes it. The next host's custody take continues
/// the parked child once by itself, under hold; a second take continues
/// nothing. (A truly running child is handed off and continued through
/// the product's child route by the offline TUI test and the
/// `handoff_resume_child` sweep shape.)
#[test]
fn a_handed_off_tree_continues_its_parked_child_once() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
    let tree_id = config.session.id.clone();
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the open", idle);
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let fixture = Arc::new(crate::agent::RecoveryFixture::new(&tree_id));
    let [running, parked, spent] = tokio.block_on(fixture.handoff_children());
    let recorded = Arc::new(Mutex::new(None));
    {
        let (fixture, recorded) = (Arc::clone(&fixture), Arc::clone(&recorded));
        super::host_loop::handoff_hooks::after_record(move || {
            *recorded.lock().unwrap() = Some(fixture.stored_continuation_causes());
        });
    }
    actions.request_handoff_and_release();
    let _ = host.join().unwrap();
    let recorded = recorded.lock().unwrap().clone().expect("the handoff ran");
    assert_eq!(
        recorded[&running], None,
        "a turn its runtime already ended is not handed off"
    );
    assert_eq!(recorded[&parked], Some(ContinuationCause::Handoff));
    assert_eq!(
        recorded[&spent], None,
        "a spent attempt holds no continuation"
    );

    tokio.block_on(async {
        fixture.next_host_take().await;
        let (decisions, _) = fixture.continuations();
        assert_eq!(decisions, 1, "one continuation, without a Resume");
        fixture.continuations_settled(1).await;
        fixture.next_host_take().await;
        let (decisions, commands) = fixture.continuations();
        assert_eq!(decisions, 1, "a second take decides nothing again");
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert_eq!(
            fixture.status(&spent),
            crate::agent_tree_protocol::NodeStatus::Interrupted
        );
        crate::topology_runtime::force_release(&tree_id).await;
    });
}

/// The planned-handoff flag is armed with its release request and taken
/// by the release: a plain release request disarms it, so a later abandon
/// is never a handoff.
#[test]
fn a_planned_handoff_is_armed_only_by_its_own_release() {
    let (sender, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    assert!(!receiver.take_planned_handoff());
    sender.request_handoff_and_release();
    assert!(receiver.take_planned_handoff());
    assert!(!receiver.take_planned_handoff(), "taken once");
    sender.request_handoff_and_release();
    sender.request_abandon_and_release();
    assert!(
        !receiver.take_planned_handoff(),
        "a plain release disarms it"
    );
}

/// A stop racing a crash: the operator stops the running root turn, and
/// the host is lost before the turn's terminal. The stop was recorded
/// before the cancel, so the next open records no loss for that turn and,
/// even under `resume_after_loss = "continue"`, nothing continues it.
#[test]
fn a_stop_the_host_lost_before_its_terminal_is_never_continued() {
    let root = tempfile::tempdir().unwrap();
    let lost =
        release_a_running_root_turn(root.path(), Release::StopThenLost, true, |config| config);
    let mut config = next_host(
        root.path(),
        &lost,
        serde_json::json!([{"output": {"text": "never"}}]),
        Arc::clone(&lost.store),
    );
    config.resume_after_loss = crate::runtime::ResumeAfterLoss::Continue;
    let seen = open_and_quit(config);
    assert!(!seen.iter().any(held_notice), "{seen:#?}");
    assert_eq!(dispatches(&lost.evidence), 1, "nothing continued the stop");
    let intents = lost.store.load_intents(&lost.id).unwrap();
    assert_eq!(
        intents
            .root_interrupts
            .iter()
            .map(|intent| intent.value.cause)
            .collect::<Vec<_>>(),
        [RootInterruptCause::Operator],
        "the stop is final for its turn"
    );
    assert_eq!(intents.root_continuations.len(), 0, "nothing continues");
}

/// An idle interrupt keeps a held root continuation, so a stray Esc never
/// discards it; a stop of everything ends it with a `stopped` record. The
/// stop finds the hold still there only if the interrupt kept it.
#[test]
fn an_idle_interrupt_keeps_a_held_root_and_a_stop_of_everything_ends_it() {
    let root = tempfile::tempdir().unwrap();
    let lost = release_a_running_root_turn(root.path(), Release::Lost, true, |config| config);
    let config = next_host(
        root.path(),
        &lost,
        serde_json::json!([]),
        Arc::clone(&lost.store),
    );
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the held notice", held_notice);
    send_attached_action(&actions, UserAction::Interrupt);
    send_attached_action(&actions, UserAction::StopAllTopology);
    wait_for(&events, &mut seen, "the stop", |event| {
        matches!(event, crate::surface_protocol::HostEvent::Row(
            plakous_host_protocol::views::transcript::TranscriptRow::Notice(notice))
            if notice.text.starts_with("stopped: the turn the lost host was running"))
    });
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
    let intents = lost.store.load_intents(&lost.id).unwrap();
    assert_eq!(
        intents
            .root_interrupts
            .iter()
            .map(|intent| intent.value.cause)
            .collect::<Vec<_>>(),
        [RootInterruptCause::OwnerLost, RootInterruptCause::Stopped],
        "the interrupt recorded nothing; the stop ended the hold"
    );
    let record = lost.store.load(&lost.id).unwrap();
    assert_eq!(root_loss_state(lost.store.as_ref(), &record).unwrap(), None);
    assert_eq!(dispatches(&lost.evidence), 1);
}
