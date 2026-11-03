use crate::runtime::test_kit::BlockOn as _;
use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

use super::*;

use crate::interrupt::{InterruptWatch, InterruptWatchState, run_watched};
use crate::runtime::test_support::*;
use plakous::application::ApplicationExecutor;
use std::cell::RefCell;
use std::time::Duration;

#[test]
fn one_shot_drive_does_not_refuse_a_live_tree_under_contention() {
    use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
    use std::sync::{Arc, Mutex, mpsc};

    struct ReleaseAfterGate(Mutex<Option<mpsc::Sender<()>>>);
    impl StoreFaults for ReleaseAfterGate {
        fn storage_unavailable(&self) -> bool {
            if let Some(release) = self.0.lock().unwrap().take() {
                release.send(()).unwrap();
            }
            false
        }
        fn before(&self, _: StoreCall, _: Option<&sessions::SessionRecord>) -> Result<(), String> {
            Ok(())
        }
    }

    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let id = config.session.id.clone();
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        &id,
        "root",
        "owner",
        config.cwd.clone(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let runtime = crate::topology_runtime::insert_root(id, tree, None)
        .block()
        .unwrap();
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (sender, _events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(config, &receiver, Sender::over(sender), None).unwrap();
    send_attached_action(
        &actions,
        UserAction::Prompt("one shot with a busy tree".into()),
    );
    let mut state = super::super::oneshot::OneShotState {
        attempted: false,
        resume_tree: false,
        result: None,
        run: None,
    };
    std::thread::scope(|scope| {
        let (locked, ready) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let runtime = Arc::clone(&runtime);
        let holder = scope.spawn(move || {
            runtime
                .read(|_| {
                    locked.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(30)).unwrap();
                })
                .unwrap();
        });
        ready.recv_timeout(Duration::from_secs(30)).unwrap();
        // The first storage probe in drive's loop follows its one-shot
        // admission check. Release there, before later tree projections.
        host.config.store = Arc::new(FaultStore::new(
            host.config.store.clone(),
            ReleaseAfterGate(Mutex::new(Some(release))),
        ));
        host.drive(Some(&mut state)).unwrap();
        holder.join().unwrap();
    });
    assert!(
        matches!(
            state.result,
            Some(Ok(super::super::oneshot::OneShotOutcome::Completed { .. }))
        ),
        "a busy live tree is not restored-paused: {:?}",
        state.result
    );
    host.release().unwrap();
}

#[test]
fn unavailable_session_workflows_never_guess_or_mutate() {
    let (events, received) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(events);
    emit_unavailable_context(&events);
    assert!(matches!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        HostEvent::Usage(usage)
            if usage.prepared_input.is_none()
                && usage.admitted_ceiling.is_none()
                && usage.status == "unavailable"
    ));
}

/// An action the operator already sent outranks the durable queue, so
/// the drive loop asks before it starts a queued turn.
#[test]
fn a_deferred_action_is_visible_before_the_queue_opens_a_turn() {
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    assert!(!state.borrow().has_deferred());
    actions.send(UserAction::Quit).unwrap();
    watch.poll();
    assert!(state.borrow().has_deferred());
    assert!(matches!(
        state.borrow_mut().take_deferred().as_deref(),
        Some(UserAction::Quit)
    ));
    assert!(!state.borrow().has_deferred());
}

#[test]
fn run_watched_observes_an_interrupt_within_one_round_trip() {
    let executor = ApplicationExecutor::new().unwrap();
    let (actions_tx, actions_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let interrupt_state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
    watch.begin();

    let sender = actions_tx;
    let round_trip = async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        sender.send(UserAction::Prompt("deferred".into())).unwrap();
        sender.send(UserAction::Interrupt).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        "receipt"
    };
    let receipt = run_watched(&executor, watch, round_trip).unwrap();

    // The round-trip still reached its own receipt; only the next step is
    // skipped by the caller.
    assert_eq!(receipt, "receipt");
    assert!(watch.interrupted());
    // Ordinary product work drained on the way to the urgent lane is
    // handed back to the host loop instead of being swallowed.
    let deferred = interrupt_state.borrow_mut().take_deferred();
    assert_eq!(
        deferred.map(|delivery| delivery.into_parts().0),
        Some(UserAction::Prompt("deferred".into()))
    );
}

#[test]
fn uninterrupted_compact_and_fork_still_run_their_host_workflows() {
    let root = tempfile::tempdir().unwrap();
    // This injected compact can fail without a fact. Fork success must
    // not depend on discarding that other workflow's unresolved request.
    let mut events = run_attached_actions(
        no_effort_openrouter_injected_host(&root.path().join("compact")),
        vec![UserAction::Compact],
    );
    assert!(
        events.iter().any(|event| matches!(event,
            HostEvent::ToolLine { name, state, .. } if name == "compact" && state == "running"
        )),
        "an uninterrupted compact still dispatches: {events:?}"
    );
    events.extend(run_attached_actions(
        no_effort_openrouter_injected_host(&root.path().join("fork")),
        vec![UserAction::ForkSession {
            account: None,
            compact: false,
        }],
    ));

    assert!(
        events.iter().any(|event| matches!(
            event,
            HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("forked session")
        )),
        "an uninterrupted fork still commits: {events:?}"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("before its canonical dispatch")
        )),
        "no interrupt checkpoint may fire without an interrupt: {events:?}"
    );
}
