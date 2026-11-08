//! A host's stop of a session (spec 02 §4.4, spec 03 §14.1): through the
//! session's mailbox and through a wire connection, on a running turn, on a
//! held continuation, repeated, against the idle release, and on a session
//! that went dormant. Every wait is for an event or a settled stop, bounded.

#![allow(clippy::unwrap_used, reason = "test-owned hosts and stores")]

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::{ClientFrame, NodeAddress, Op, encode_line};
use plakous_host_protocol::{IntentSender, StopTicket};

use crate::host_service::AttachmentCount;
use crate::host_service::wire::{Face, OpenedSession, SessionSlot, WireSession};
use crate::runtime::recover_tests::{script, scripted_host_with, wait_for};
use crate::runtime::root_loss::root_loss_state;
use crate::runtime::root_loss_open_tests::{
    Cancel, Lost, Release, dispatches, held_notice, next_host, open_and_quit,
    release_a_running_root_turn,
};
use crate::runtime::test_kit::BlockOn as _;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::send_attached_action;
use crate::runtime::{IdleRelease, ResumeAfterLoss, spawn_host_worker};
use crate::sessions::projector::RootInterruptCause;

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

/// What the session settled the stop `ticket` to, within a bound.
fn settled(actions: &IntentSender<TestExtension>, ticket: StopTicket) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(outcome) = actions.stop_settled(ticket) {
            return outcome;
        }
        assert!(Instant::now() < deadline, "the stop was never settled");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn root_causes(lost: &Lost) -> Vec<RootInterruptCause> {
    lost.store
        .load_intents(&lost.id)
        .unwrap()
        .root_interrupts
        .iter()
        .map(|intent| intent.value.cause)
        .collect()
}

fn stopped_note(event: &HostEvent) -> bool {
    matches!(event, HostEvent::Row(
        plakous_host_protocol::views::transcript::TranscriptRow::Notice(notice))
        if notice.text.starts_with("stopped: the session's work ended"))
}

#[test]
fn a_launcher_release_fence_refuses_a_running_turn_without_cancelling_it() {
    let root = tempfile::tempdir().unwrap();
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "steps": [{"stream": [{"text": "still running"}, {"stall":30000}]}]
        }))
        .unwrap(),
    );
    let (config, _) = scripted_host_with(root.path(), plan);
    let session = config.session.id.clone();
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("work".into()));
    wait_for(
        &events,
        &mut seen,
        "running",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("still running")),
    );
    let ticket = actions.request_release_for_reopen(&session).unwrap();
    assert!(settled(&actions, ticket).is_err());
    assert!(!host.is_finished());
    assert!(!seen.iter().any(|event| matches!(event, HostEvent::TurnLifecycle(lifecycle) if format!("{lifecycle:?}").contains("Cancelled"))));
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
}

#[test]
fn a_stale_launcher_fence_never_releases_a_successor_session() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
    let session = config.session.id.clone();
    let (actions, _events, host) = spawn_host_worker(config).unwrap();
    let stale = actions
        .request_release_for_reopen("a-session-this-actor-does-not-serve")
        .unwrap();
    assert!(settled(&actions, stale).is_err());
    assert!(!host.is_finished());
    let own = actions.request_release_for_reopen(&session).unwrap();
    assert_eq!(settled(&actions, own), Ok(()));
    host.join().unwrap().unwrap();
}

#[test]
fn a_dormant_directory_stop_never_opens_or_dispatches_its_continuation() {
    for release in [Release::Lost, Release::Handoff] {
        let root = tempfile::tempdir().unwrap();
        let lost = release_a_running_root_turn(root.path(), release, Cancel::Deaf, |config| config);
        let before = dispatches(&lost.evidence);
        let mut config = next_host(
            root.path(),
            &lost,
            serde_json::json!([]),
            Arc::clone(&lost.store),
        );
        let mut record = lost.store.load(&lost.id).unwrap();
        super::stop_dormant_session(&config.state_roots, lost.store.as_ref(), &mut record)
            .block()
            .unwrap();
        let causes = root_causes(&lost);
        let revision = lost.store.load(&lost.id).unwrap().store_revision;
        assert_eq!(causes.last(), Some(&RootInterruptCause::Stopped));
        super::stop_dormant_session(&config.state_roots, lost.store.as_ref(), &mut record)
            .block()
            .unwrap();
        assert_eq!(
            root_causes(&lost),
            causes,
            "repeat stop changed the durable verdict"
        );
        assert_eq!(lost.store.load(&lost.id).unwrap().store_revision, revision);
        assert_eq!(
            dispatches(&lost.evidence),
            before,
            "a stop opened a paid continuation"
        );
        config.session = lost.store.load(&lost.id).unwrap();
        config.resume_after_loss = ResumeAfterLoss::Continue;
        let seen = open_and_quit(config);
        assert!(!seen.iter().any(held_notice), "{seen:#?}");
        assert_eq!(
            dispatches(&lost.evidence),
            before,
            "a stopped root was redispatched"
        );
    }
}

#[test]
fn a_dormant_stop_records_cancellation_before_an_unavailable_runners_settlement() {
    use plakous::runner::{ConfiguredRunner, PlacementFailure, ProbePhase, RunnerRegistry};
    let root = tempfile::tempdir().unwrap();
    let lost =
        release_a_running_root_turn(root.path(), Release::Lost, Cancel::Deaf, |config| config);
    let before = dispatches(&lost.evidence);
    let registry = RunnerRegistry::default();
    registry.configure(ConfiguredRunner {
        runner_id: "saved-runner".into(),
        names: vec!["remote".into()],
    });
    registry.blocked(
        "saved-runner",
        PlacementFailure::RunnerUnreachable {
            phase: ProbePhase::LinkLost,
            timed_out: false,
        },
        1,
    );
    let roots = lost
        .state_roots
        .clone()
        .for_runner_stop(registry.clone(), "saved-runner");
    let mut record = lost.store.load(&lost.id).unwrap();
    let sessions = roots.runtime_sessions().unwrap();
    let line = sessions
        .line_head_of(record.plakous_locator.unwrap())
        .block()
        .unwrap();
    let held = sessions
        .open_lease_only(
            &plakous::sessions::SessionOperationId::new("foreign:held").unwrap(),
            line.line(),
        )
        .block()
        .unwrap();
    assert!(
        super::stop_dormant_session(&roots, lost.store.as_ref(), &mut record)
            .block()
            .is_err(),
        "a new cancellation bypassed ownership"
    );
    assert!(!root_causes(&lost).contains(&RootInterruptCause::Stopped));
    held.release().block().unwrap();
    super::stop_dormant_session(&roots, lost.store.as_ref(), &mut record)
        .block()
        .unwrap();
    assert!(roots.runner_pending());
    assert!(registry.needs_connection("saved-runner"));
    assert_eq!(
        root_causes(&lost).last(),
        Some(&RootInterruptCause::Stopped)
    );
    assert!(lost.store.load(&lost.id).unwrap().child_wake_suppressed);
    let revision = lost.store.load(&lost.id).unwrap().store_revision;
    let sessions = roots.runtime_sessions().unwrap();
    let line = sessions
        .line_head_of(record.plakous_locator.unwrap())
        .block()
        .unwrap();
    let held = sessions
        .open_lease_only(
            &plakous::sessions::SessionOperationId::new("reconcile:held").unwrap(),
            line.line(),
        )
        .block()
        .unwrap();
    super::stop_dormant_session(&roots, lost.store.as_ref(), &mut record)
        .block()
        .unwrap();
    held.release().block().unwrap();
    assert_eq!(lost.store.load(&lost.id).unwrap().store_revision, revision);
    assert_eq!(dispatches(&lost.evidence), before);
    let mut next = next_host(
        root.path(),
        &lost,
        serde_json::json!([]),
        Arc::clone(&lost.store),
    );
    next.session = lost.store.load(&lost.id).unwrap();
    next.resume_after_loss = ResumeAfterLoss::Continue;
    let seen = open_and_quit(next);
    assert!(!seen.iter().any(held_notice));
    assert_eq!(
        dispatches(&lost.evidence),
        before,
        "recovery redispatched a stopped turn"
    );
}

/// A stop of a root that holds its continuation after a loss records
/// `stopped` on the loss's turn before it is settled, and a repeated stop
/// records nothing more. The next open, under `continue`, holds nothing and
/// runs nothing.
#[test]
fn a_stop_ends_a_held_continuation_and_a_repeated_stop_records_nothing_more() {
    let root = tempfile::tempdir().unwrap();
    let lost =
        release_a_running_root_turn(root.path(), Release::Lost, Cancel::Deaf, |config| config);
    let config = next_host(
        root.path(),
        &lost,
        serde_json::json!([]),
        Arc::clone(&lost.store),
    );
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the held notice", held_notice);
    let first = actions.request_stop().unwrap();
    assert_eq!(settled(&actions, first), Ok(()));
    assert_eq!(
        root_causes(&lost),
        [RootInterruptCause::OwnerLost, RootInterruptCause::Stopped],
        "the stop is durable once settled"
    );
    let again = actions.request_stop().unwrap();
    assert_eq!(settled(&actions, again), Ok(()));
    assert_eq!(
        root_causes(&lost),
        [RootInterruptCause::OwnerLost, RootInterruptCause::Stopped],
        "a repeated stop records nothing more"
    );
    wait_for(&events, &mut seen, "the stop's notice", stopped_note);
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
    let mut config = next_host(
        root.path(),
        &lost,
        serde_json::json!([{"output": {"text": "must not run"}}]),
        Arc::clone(&lost.store),
    );
    config.resume_after_loss = ResumeAfterLoss::Continue;
    let seen = open_and_quit(config);
    assert!(!seen.iter().any(held_notice), "{seen:#?}");
    assert_eq!(dispatches(&lost.evidence), 1, "nothing continued the stop");
    let record = lost.store.load(&lost.id).unwrap();
    assert_eq!(root_loss_state(lost.store.as_ref(), &record).unwrap(), None);
}

/// A stop of a running root turn records `stopped` on the turn before its
/// cancel, ends the turn, and is settled after the turn ended.
#[test]
fn a_stop_of_a_running_turn_records_it_first_and_settles_after_the_turn() {
    let root = tempfile::tempdir().unwrap();
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "steps": [{"stream": [{"text": "running"}, {"stall": 30000}]}]
        }))
        .unwrap(),
    );
    let (config, _) = scripted_host_with(root.path(), plan);
    let id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("work".into()));
    wait_for(
        &events,
        &mut seen,
        "the running turn",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("running")),
    );
    let ticket = actions.request_stop().unwrap();
    assert_eq!(settled(&actions, ticket), Ok(()));
    seen.extend(events.try_iter());
    assert!(
        seen.iter().any(|event| matches!(event,
            HostEvent::TurnLifecycle(lifecycle) if format!("{:?}", lifecycle.phase) == "Cancelled")),
        "the stop was settled before its turn ended: {seen:#?}"
    );
    let causes: Vec<_> = store
        .load_intents(&id)
        .unwrap()
        .root_interrupts
        .iter()
        .map(|intent| (intent.value.cause, intent.value.anchor.clone()))
        .collect();
    assert_eq!(causes.len(), 1, "{causes:?}");
    assert_eq!(causes[0].0, RootInterruptCause::Stopped);
    assert!(
        matches!(
            causes[0].1,
            crate::sessions::projector::RootStopAnchor::Turn { .. }
        ),
        "{causes:?}"
    );
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
}

/// A busy stop cancels the root turn while a slow child is still retiring.
/// Joining that child first would hold the cancel for the shutdown bound.
#[test]
fn a_busy_stop_cancels_while_a_slow_child_is_still_retiring() {
    let root = tempfile::tempdir().unwrap();
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "steps": [{"stream": [{"text": "running"}, {"stall": 30000}]}]
        }))
        .unwrap(),
    );
    let (config, _) = scripted_host_with(root.path(), plan);
    let id = config.session.id.clone();
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("work".into()));
    wait_for(
        &events,
        &mut seen,
        "the running turn",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("running")),
    );
    let child = SlowRetire::new(false);
    let _hub = crate::runtime::test_kit::install_idle_topology_hub(&id, root.path());
    occupy_running_child(&id, child.handle());
    let started = Instant::now();
    let _ticket = actions.request_stop().unwrap();
    let bound = crate::topology_runtime::HOST_SHUTDOWN_RELEASE_TIMEOUT / 2;
    wait_for_bounded(
        &events,
        &mut seen,
        bound,
        "the turn's cancel",
        |event| matches!(event, HostEvent::TurnLifecycle(lifecycle) if format!("{lifecycle:?}").contains("Cancelled")),
    );
    assert!(
        started.elapsed() < bound,
        "the cancel waited for the child: {:?}",
        started.elapsed()
    );
    assert!(
        child.started() && !child.finished(),
        "the cancel did not land during the child's retirement"
    );
    child.release();
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
}

/// A turn that finishes during the tree stop is not anchored `stopped`.
#[test]
fn a_turn_that_finishes_during_the_tree_stop_is_not_anchored_stopped() {
    let root = tempfile::tempdir().unwrap();
    let gate = plakous_offline_crash_harness::gate::GateHandle::new();
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "steps": [{
                "stream": [{"text": "running"}, {"gate": gate.script()}],
                "output": {"text": "done"}
            }]
        }))
        .unwrap(),
    );
    let (config, _) = scripted_host_with(root.path(), plan);
    let id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let roots = config.state_roots.clone();
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("work".into()));
    wait_for(
        &events,
        &mut seen,
        "the running turn",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("running")),
    );
    let child = SlowRetire::new(true);
    let _hub = crate::runtime::test_kit::install_idle_topology_hub(&id, root.path());
    occupy_running_child(&id, child.handle());
    let ticket = actions.request_stop().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !child.started() {
        assert!(
            Instant::now() < deadline,
            "the tree stop did not reach the child"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    gate.release();
    while !turn_terminal_applied(store.as_ref(), &roots, &id) {
        assert!(
            Instant::now() < deadline,
            "the turn did not finish during the tree stop: {seen:#?}"
        );
        seen.extend(events.try_iter());
        std::thread::sleep(Duration::from_millis(20));
    }
    child.release();
    assert_eq!(settled(&actions, ticket), Ok(()));
    let causes: Vec<_> = store
        .load_intents(&id)
        .unwrap()
        .root_interrupts
        .iter()
        .map(|intent| intent.value.cause)
        .collect();
    assert!(
        !causes.contains(&RootInterruptCause::Stopped),
        "a finished turn was anchored stopped: {causes:?}"
    );
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
}

/// A host lost after the stop was recorded and before its turn's terminal
/// leaves a stop that nothing continues, under `continue`.
#[test]
fn a_stop_the_host_lost_before_the_turn_ended_is_never_continued() {
    let root = tempfile::tempdir().unwrap();
    let lost = release_a_running_root_turn(
        root.path(),
        Release::HostStopThenLost,
        Cancel::Deaf,
        |config| config,
    );
    let mut config = next_host(
        root.path(),
        &lost,
        serde_json::json!([{"output": {"text": "must not run"}}]),
        Arc::clone(&lost.store),
    );
    config.resume_after_loss = ResumeAfterLoss::Continue;
    let seen = open_and_quit(config);
    assert!(!seen.iter().any(held_notice), "{seen:#?}");
    assert_eq!(dispatches(&lost.evidence), 1, "nothing continued the stop");
    assert_eq!(root_causes(&lost), [RootInterruptCause::Stopped]);
}

/// A client's output, readable while the connection writes to it.
#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A remote client's `stop_session` of `session`'s own session, through a
/// connection of its own: the outcome it is answered.
fn stop_over_the_wire(session: &WireSession<TestExtension>) -> serde_json::Value {
    let output = Output::default();
    let mut connection = session.connect(output.clone(), Face::Remote, || false);
    let frame = ClientFrame::<TestExtension>::Request {
        id: 7,
        op: Op::StopSession {
            session: session.session_id(),
        },
    };
    assert!(connection.frame(Ok(encode_line(&frame).unwrap())).unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        connection.flush().unwrap();
        let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        if let Some(response) = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|frame| frame["frame"] == "response" && frame["id"] == 7)
        {
            connection.leave();
            return response["outcome"].clone();
        }
        assert!(Instant::now() < deadline, "the stop was never answered");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A session opened for a slot whose host releases it at once when idle.
fn released_at_once(
    mut config: crate::runtime::HostConfig<TestExtension>,
) -> crate::CliResult<OpenedSession<TestExtension>> {
    let attachments = AttachmentCount::default();
    config.idle_release = Some(IdleRelease {
        after: Duration::ZERO,
        attachments: attachments.clone(),
    });
    let session_id = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config)?;
    Ok(OpenedSession {
        runner_pending: None,
        root: NodeAddress {
            tree_id: session_id.clone(),
            agent_id: session_id.clone(),
        },
        session_id,
        actions,
        events,
        worker,
        attachments,
        roots: Box::new(|_| None),
    })
}

fn until_dormant(slot: &SessionSlot<TestExtension>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !slot.dormant() {
        assert!(
            Instant::now() < deadline,
            "the idle session was not released"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The connection that stops a session holds it resident, so an idle
/// release of zero never races the stop: it is answered `stopped` on the
/// session it was handed, which releases only once the connection's hold
/// ends. A session that went dormant is woken by the next connection, and
/// its stop reaches the reopened session.
#[test]
fn a_stop_never_races_the_idle_release_and_reaches_a_woken_session() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("served");
    std::fs::create_dir_all(&workspace).unwrap();
    let (config, _) = scripted_host_with(
        &workspace,
        script(serde_json::json!([{"output": {"text": "stored"}}])),
    );
    let store = Arc::clone(&config.store);
    let state_roots = config.state_roots.clone();
    let opened = released_at_once(config).unwrap();
    // The first turn stores the session, so it opens again by id; it is
    // taken before the actor's first idle decision, and it ends before the
    // slot serves the session, so the stop has no queue to pause.
    send_attached_action(&opened.actions, UserAction::Prompt("store it".into()));
    wait_for(
        &opened.events,
        &mut Vec::new(),
        "the storing turn",
        crate::runtime::recover_tests::turn_completed,
    );
    let id = opened.session_id.clone();
    let slot = SessionSlot::new(
        opened,
        crate::runtime::idle_release_tests::reopener(workspace, store, state_roots, Duration::ZERO),
    );
    let (session, hold) = slot.resident().unwrap();
    assert_eq!(
        stop_over_the_wire(&session),
        serde_json::json!({"ok": {"answer": "stopped", "runner_pending": false}})
    );
    assert!(!slot.dormant(), "the session released under its holder");
    drop(hold);
    drop(session);
    until_dormant(&slot);
    let (woken, hold) = slot.resident().unwrap();
    assert_eq!(woken.session_id(), id);
    assert_eq!(
        stop_over_the_wire(&woken),
        serde_json::json!({"ok": {"answer": "stopped", "runner_pending": false}})
    );
    drop(hold);
    drop(woken);
    slot.quit(Duration::from_secs(30)).unwrap();
}

/// A held root continuation does not keep a session resident, so the
/// session goes dormant holding it; a stop then wakes it, records `stopped`
/// before it is answered, and nothing continues it at a later open under
/// `continue`.
#[test]
fn a_stop_of_a_dormant_session_ends_its_held_continuation() {
    let root = tempfile::tempdir().unwrap();
    let lost =
        release_a_running_root_turn(root.path(), Release::Lost, Cancel::Deaf, |config| config);
    let open = {
        let root = root.path().to_path_buf();
        let store = Arc::clone(&lost.store);
        let state_roots = lost.state_roots.clone();
        let id = lost.id.clone();
        move |requested: &str| {
            assert_eq!(requested, id);
            let (mut config, _) = scripted_host_with(&root, script(serde_json::json!([])));
            config.state_roots = state_roots.clone();
            config.store = Arc::clone(&store);
            config.session = store.load(requested).unwrap();
            released_at_once(config)
        }
    };
    let slot = SessionSlot::new(open(&lost.id).unwrap(), open);
    until_dormant(&slot);
    let record = lost.store.load(&lost.id).unwrap();
    assert!(
        root_loss_state(lost.store.as_ref(), &record)
            .unwrap()
            .is_some(),
        "the dormant session holds its continuation"
    );
    let (woken, hold) = slot.resident().unwrap();
    assert_eq!(
        stop_over_the_wire(&woken),
        serde_json::json!({"ok": {"answer": "stopped", "runner_pending": false}})
    );
    assert_eq!(
        root_causes(&lost),
        [RootInterruptCause::OwnerLost, RootInterruptCause::Stopped]
    );
    drop(hold);
    drop(woken);
    slot.quit(Duration::from_secs(30)).unwrap();
    let mut config = next_host(
        root.path(),
        &lost,
        serde_json::json!([{"output": {"text": "must not run"}}]),
        Arc::clone(&lost.store),
    );
    config.resume_after_loss = ResumeAfterLoss::Continue;
    let seen = open_and_quit(config);
    assert!(!seen.iter().any(held_notice), "{seen:#?}");
    assert_eq!(dispatches(&lost.evidence), 1, "nothing continued the stop");
}

/// The tuple once a stop's notice commit, if it has one, has landed.
fn stable_stop_tuple(
    store: &dyn crate::session_store::SessionRecordStore,
    id: &str,
) -> (usize, usize, usize, u64) {
    let mut seen = stop_tuple(store, id);
    let mut quiet = Instant::now() + Duration::from_millis(150);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let next = stop_tuple(store, id);
        if next != seen {
            seen = next;
            quiet = Instant::now() + Duration::from_millis(150);
        } else if Instant::now() >= quiet {
            return seen;
        }
        assert!(Instant::now() < deadline, "the stop record never settled");
    }
}

fn stop_tuple(
    store: &dyn crate::session_store::SessionRecordStore,
    id: &str,
) -> (usize, usize, usize, u64) {
    let intents = store.load_intents(id).unwrap();
    let record = store.load(id).unwrap();
    (
        intents.wake_suppressions.len(),
        intents.pause_causes.len(),
        intents.root_interrupts.len(),
        record.store_revision,
    )
}

/// An idle stored session, stopped three times, grows no wake row, pause,
/// root interrupt or record revision after the first stop.
#[test]
fn a_repeated_idle_stop_records_nothing_new() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([{"output": {"text": "stored"}}])),
    );
    let id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    // The host marks this hub. A session with no child never installs one,
    // so the repeated stop would not reach the tree.
    let runtime = crate::runtime::test_kit::install_idle_topology_hub(&id, root.path());
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("store it".into()));
    wait_for(
        &events,
        &mut seen,
        "the storing turn",
        crate::runtime::recover_tests::turn_completed,
    );
    let before = stop_tuple(store.as_ref(), &id);
    runtime
        .mutate_then_emit(false, |tree| {
            use crate::agent_tree::{AgentCommand, AgentCommandKind};
            use crate::agent_tree_protocol::SpawnAgentRequest;
            let child = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map_err(|error| error.message)?
                .agent_id;
            tree.bind_session_locator(&child, "locator".to_owned())
                .map_err(|error| error.message)?;
            tree.set_active_turn(&child, true)
                .map_err(|error| error.message)?;
            tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-0000000000b1")
                .map_err(|error| error.message)?;
            let attempt = tree.node(&child).unwrap().attempt_id.clone();
            tree.record_command(AgentCommand::intent(
                format!("start:{child}"),
                &child,
                attempt,
                AgentCommandKind::Start,
                "assignment",
                Vec::new(),
            ))
            .map_err(|error| error.message)?;
            Ok(())
        })
        .block()
        .unwrap();
    let first = actions.request_stop().unwrap();
    assert_eq!(settled(&actions, first), Ok(()));
    let revision = runtime.read(|tree| tree.revision).unwrap();
    let after_first = stable_stop_tuple(store.as_ref(), &id);
    assert!(
        after_first.0 > before.0,
        "the first stop suppressed wakes: {before:?} -> {after_first:?}"
    );
    let second = actions.request_stop().unwrap();
    assert_eq!(settled(&actions, second), Ok(()));
    let after_second = stable_stop_tuple(store.as_ref(), &id);
    let third = actions.request_stop().unwrap();
    assert_eq!(settled(&actions, third), Ok(()));
    let after_third = stable_stop_tuple(store.as_ref(), &id);
    // The product revision may move when the first stop's tree snapshot
    // commits. The stop's own rows do not.
    let rows = |tuple: (usize, usize, usize, u64)| (tuple.0, tuple.1, tuple.2);
    assert_eq!(
        rows(after_second),
        rows(after_first),
        "the second stop wrote"
    );
    assert_eq!(rows(after_third), rows(after_first), "the third stop wrote");
    assert_eq!(
        runtime.read(|tree| tree.revision).unwrap(),
        revision,
        "a repeated stop marked the tree again"
    );
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
}

/// A crash at each write before the root record leaves an ordinary loss,
/// which continues under `continue`. A crash after the root record leaves
/// a stop nothing continues.
#[test]
fn a_crash_around_the_root_stop_record() {
    for at in ["wake", "tree", "queue", "process", "root"] {
        let root = tempfile::tempdir().unwrap();
        let lost =
            release_a_running_root_turn(root.path(), Release::Lost, Cancel::Deaf, |config| config);
        crash_at(root.path(), &lost, at);
        let mut config = next_host(
            root.path(),
            &lost,
            serde_json::json!([{"output": {"text": "continued"}}]),
            Arc::clone(&lost.store),
        );
        config.resume_after_loss = ResumeAfterLoss::Continue;
        let (actions, events, host) = spawn_host_worker(config).unwrap();
        let mut seen = Vec::new();
        wait_for(
            &events,
            &mut seen,
            &format!("{at}: the continuation"),
            turnish,
        );
        send_attached_action(&actions, UserAction::Quit);
        host.join().unwrap().unwrap();
        assert_eq!(
            dispatches(&lost.evidence),
            2,
            "{at}: nothing continued the ordinary loss"
        );
    }
    let root = tempfile::tempdir().unwrap();
    let lost =
        release_a_running_root_turn(root.path(), Release::Lost, Cancel::Deaf, |config| config);
    crash_at(root.path(), &lost, "after");
    assert!(
        root_causes(&lost).contains(&RootInterruptCause::Stopped),
        "the root record is durable past the cut"
    );
    let mut config = next_host(
        root.path(),
        &lost,
        serde_json::json!([{"output": {"text": "must not run"}}]),
        Arc::clone(&lost.store),
    );
    config.resume_after_loss = ResumeAfterLoss::Continue;
    let seen = open_and_quit(config);
    assert!(!seen.iter().any(held_notice), "{seen:#?}");
    assert!(!seen.iter().any(turnish), "the stop continued: {seen:#?}");
    assert_eq!(dispatches(&lost.evidence), 1, "the stop continued");
}

/// What one stop-cut must leave behind.
struct CutExpect {
    at: &'static str,
    suppression: bool,
    children_stopped: bool,
    root_stopped: bool,
    continues: bool,
    wakes: bool,
}

/// Idle and busy cuts, with a running child, a held child and a pending
/// child result. Before the root record the root is an ordinary loss; after
/// it nothing continues, the children are stopped once the tree cut has
/// passed, and a child result does not wake the root once suppression is
/// durable.
#[test]
fn a_stop_cut_with_children_decides_the_root_and_the_wake() {
    let idle = [
        CutExpect {
            at: "wake",
            suppression: false,
            children_stopped: false,
            root_stopped: false,
            continues: true,
            wakes: true,
        },
        CutExpect {
            at: "tree",
            suppression: true,
            children_stopped: false,
            root_stopped: false,
            continues: true,
            wakes: false,
        },
        CutExpect {
            at: "queue",
            suppression: true,
            children_stopped: true,
            root_stopped: false,
            continues: true,
            wakes: false,
        },
        CutExpect {
            at: "process",
            suppression: true,
            children_stopped: true,
            root_stopped: false,
            continues: true,
            wakes: false,
        },
        CutExpect {
            at: "root",
            suppression: true,
            children_stopped: true,
            root_stopped: false,
            continues: true,
            wakes: false,
        },
        CutExpect {
            at: "after",
            suppression: true,
            children_stopped: true,
            root_stopped: true,
            continues: false,
            wakes: false,
        },
    ];
    for expect in idle {
        let root = tempfile::tempdir().unwrap();
        let lost =
            release_a_running_root_turn(root.path(), Release::Lost, Cancel::Deaf, |config| config);
        let (running, held) = crash_idle(root.path(), &lost, expect.at);
        assert_cut(root.path(), &lost, "idle", &expect, &running, &held);
    }
    let busy = [
        CutExpect {
            at: "wake",
            suppression: false,
            children_stopped: false,
            root_stopped: false,
            continues: true,
            wakes: true,
        },
        CutExpect {
            at: "tree",
            suppression: true,
            children_stopped: false,
            root_stopped: false,
            continues: true,
            wakes: false,
        },
        CutExpect {
            at: "root",
            suppression: true,
            children_stopped: true,
            root_stopped: false,
            continues: true,
            wakes: false,
        },
        CutExpect {
            at: "process",
            suppression: true,
            children_stopped: true,
            root_stopped: true,
            continues: false,
            wakes: false,
        },
    ];
    for expect in busy {
        let root = tempfile::tempdir().unwrap();
        let (lost, running, held) = crash_busy(root.path(), expect.at);
        assert_cut(root.path(), &lost, "busy", &expect, &running, &held);
    }
}

fn crash_idle(root: &std::path::Path, lost: &Lost, at: &str) -> (String, String) {
    let _guard = crate::runtime::test_seams::Guard;
    crate::runtime::test_seams::arm_stop_cut(&lost.id, at);
    let config = next_host(root, lost, serde_json::json!([]), Arc::clone(&lost.store));
    let _hub = crate::runtime::test_kit::install_idle_topology_hub(&lost.id, root);
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    wait_for(&events, &mut Vec::new(), "the held notice", held_notice);
    let planted = plant_children(&lost.id);
    let _ = actions.request_stop().unwrap();
    assert!(
        host.join().is_err(),
        "{at}: the actor did not stop at the cut"
    );
    planted
}

fn crash_busy(root: &std::path::Path, at: &str) -> (Lost, String, String) {
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "stall_ignores_cancellation": true,
            "steps": [{"stream": [{"text": "running"}, {"stall": 30000}]}]
        }))
        .unwrap(),
    );
    let (config, evidence) = scripted_host_with(root, plan);
    let id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let state_roots = config.state_roots.clone();
    let _guard = crate::runtime::test_seams::Guard;
    crate::runtime::test_seams::arm_stop_cut(&id, at);
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    send_attached_action(&actions, UserAction::Prompt("run".into()));
    wait_for(
        &events,
        &mut Vec::new(),
        "the running turn",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("running")),
    );
    // After the turn is in flight. Installing the hub earlier holds the
    // scripted turn off its provider.
    let _hub = crate::runtime::test_kit::install_idle_topology_hub(&id, root);
    let (running, held) = plant_children(&id);
    let _ = actions.request_stop().unwrap();
    assert!(
        host.join().is_err(),
        "{at}: the actor did not stop at the cut"
    );
    (
        Lost {
            store,
            state_roots,
            id,
            evidence,
            seen: Vec::new(),
        },
        running,
        held,
    )
}

fn wait_for_bounded(
    events: &crate::surface_protocol::HostEventReceiver<TestExtension>,
    seen: &mut Vec<HostEvent>,
    bound: Duration,
    what: &str,
    matches: impl Fn(&HostEvent) -> bool,
) {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(20)) {
            let hit = matches(&event);
            seen.push(event);
            if hit {
                return;
            }
        }
    }
    panic!("timed out waiting for {what} within {bound:?}: {seen:#?}");
}

/// A live child whose retirement the test releases. `hold_dispatch` makes the
/// tree stop itself wait, so a turn can finish before the root record.
struct SlowRetire {
    release: Arc<tokio::sync::Notify>,
    started: Arc<std::sync::atomic::AtomicBool>,
    finished: Arc<std::sync::atomic::AtomicBool>,
    hold_dispatch: bool,
}

impl SlowRetire {
    fn new(hold_dispatch: bool) -> Self {
        Self {
            release: Arc::new(tokio::sync::Notify::new()),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            finished: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hold_dispatch,
        }
    }

    fn handle(&self) -> Box<dyn crate::topology_runtime::LiveChildHandle> {
        Box::new(SlowRetireHandle {
            release: Arc::clone(&self.release),
            started: Arc::clone(&self.started),
            finished: Arc::clone(&self.finished),
            hold_dispatch: self.hold_dispatch,
        })
    }

    fn release(&self) {
        self.release.notify_one();
    }

    fn started(&self) -> bool {
        self.started.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct SlowRetireHandle {
    release: Arc<tokio::sync::Notify>,
    started: Arc<std::sync::atomic::AtomicBool>,
    finished: Arc<std::sync::atomic::AtomicBool>,
    hold_dispatch: bool,
}

impl crate::topology_runtime::LiveChildHandle for SlowRetireHandle {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        None
    }

    fn turn_id(&self) -> plakous::model::TurnId {
        "00000000-0000-0000-0000-0000000000c9".parse().unwrap()
    }

    fn turn_control(&self) -> plakous::agent::TurnControl {
        panic!("the slow child has no turn control")
    }

    fn follow_up(
        &self,
        _: plakous::agent::CommandId,
        _: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Ok(()) })
    }

    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        None
    }

    fn restart(
        &mut self,
        _: crate::agent_tree_tools::TreeDriver,
        _: plakous::agent::CommandId,
        _: String,
        _: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>,
    > {
        Box::pin(async { Err("the slow child does not restart".into()) })
    }

    fn release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        self.cancel_and_release(driver, None)
    }

    fn cancel_and_release(
        self: Box<Self>,
        _driver: crate::agent_tree_tools::TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        Box::pin(async move {
            self.started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if self.hold_dispatch {
                self.release.notified().await;
            }
            if let Some(dispatched) = dispatched {
                let _ = dispatched.send(());
            }
            if !self.hold_dispatch {
                self.release.notified().await;
            }
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            crate::topology_runtime::LiveRelease::default()
        })
    }
}

fn occupy_running_child(tree_id: &str, child: Box<dyn crate::topology_runtime::LiveChildHandle>) {
    use crate::agent_tree_protocol::SpawnAgentRequest;
    let runtime = crate::topology_runtime::lookup(tree_id).expect("the session's tree");
    let id = runtime
        .mutate_then_emit(false, |tree| {
            let id = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map_err(|error| error.message)?
                .agent_id;
            tree.set_active_turn(&id, true)
                .map_err(|error| error.message)?;
            Ok(id)
        })
        .block()
        .unwrap();
    let occupied = plakous::application::ApplicationExecutor::new()
        .unwrap()
        .block_on(runtime.try_occupy_live(id, child))
        .unwrap();
    assert!(occupied.is_ok(), "the running child was not admitted live");
}

/// Whether the views' cursor has applied some turn terminal. The busy stop
/// uses the same cursor to decide it must not anchor `stopped`.
fn turn_terminal_applied(
    store: &dyn crate::session_store::SessionRecordStore,
    roots: &crate::agent::ProductStateRoots,
    id: &str,
) -> bool {
    use plakous::sessions::feed::{FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedSeq};
    let Ok(Some(views)) = store.load_views(id) else {
        return false;
    };
    let Some(runtime_id) = views.state.runtime_session_id else {
        return false;
    };
    let through = views.cursor.get();
    if through == 0 {
        return false;
    }
    let Ok(sessions) = roots.runtime_sessions() else {
        return false;
    };
    plakous::application::ApplicationExecutor::new()
        .unwrap()
        .block_on(async move {
            let mut cursor = FeedSeq::new(0);
            loop {
                let page = sessions
                    .feed_page(FeedPageRequest::after(FeedCursor::new(runtime_id, cursor)))
                    .await;
                let Ok(page) = page else {
                    return false;
                };
                let records = match page {
                    FeedPage::Records { records, .. } => records,
                    _ => return false,
                };
                if records.is_empty() {
                    return false;
                }
                for record in records {
                    let seq = record.header().seq;
                    cursor = seq;
                    if seq.get() > through {
                        return false;
                    }
                    if record
                        .facts()
                        .iter()
                        .any(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
                    {
                        return true;
                    }
                }
            }
        })
        .unwrap()
}

fn plant_children(tree_id: &str) -> (String, String) {
    use crate::agent_tree::{AgentCommand, AgentCommandKind};
    use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest};
    let runtime = crate::topology_runtime::lookup(tree_id).expect("the session's tree");
    let planted = runtime
        .mutate_then_emit(false, |tree| {
            let spawn = |tree: &mut crate::agent_tree::ProductAgentTree| {
                tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map_err(|error| error.message)
                    .map(|spawned| spawned.agent_id)
            };
            let running = spawn(tree)?;
            let held = spawn(tree)?;
            let result = spawn(tree)?;
            for (child, turn) in [
                (running.as_str(), "00000000-0000-0000-0000-0000000000c1"),
                (held.as_str(), "00000000-0000-0000-0000-0000000000c2"),
            ] {
                tree.bind_session_locator(child, "locator".to_owned())
                    .map_err(|error| error.message)?;
                tree.set_active_turn(child, true)
                    .map_err(|error| error.message)?;
                tree.bind_attempt_turn(child, turn)
                    .map_err(|error| error.message)?;
                let attempt = tree.node(child).unwrap().attempt_id.clone();
                tree.record_command(AgentCommand::intent(
                    format!("start:{child}"),
                    child,
                    attempt,
                    AgentCommandKind::Start,
                    "assignment",
                    Vec::new(),
                ))
                .map_err(|error| error.message)?;
            }
            tree.release_attempt_turn(&held)
                .map_err(|error| error.message)?;
            tree.park_child(&held, Vec::new())
                .map_err(|error| error.message)?;
            tree.mark_owner_lost(&held);
            tree.submit_result(
                &result,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .map_err(|error| error.message)?;
            tree.record_turn_completed(&result)
                .map_err(|error| error.message)?;
            Ok((running, held))
        })
        .block()
        .unwrap();
    assert!(
        !runtime
            .read(|tree| tree.pending_mail("root").is_empty())
            .unwrap(),
        "the child result left no mail"
    );
    planted
}

fn assert_cut(
    root: &std::path::Path,
    lost: &Lost,
    path: &str,
    expect: &CutExpect,
    running: &str,
    held: &str,
) {
    let label = format!("{path} {}", expect.at);
    let intents = lost.store.load_intents(&lost.id).unwrap();
    assert_eq!(
        !intents.wake_suppressions.is_empty(),
        expect.suppression,
        "{label}: wake suppression"
    );
    assert_eq!(
        root_causes(lost).contains(&RootInterruptCause::Stopped),
        expect.root_stopped,
        "{label}: root stop"
    );
    assert_eq!(
        children_stopped(lost, running, held),
        expect.children_stopped,
        "{label}: children"
    );
    if expect.wakes {
        assert!(
            !load_tree(&lost.state_roots, &lost.id)
                .pending_mail("root")
                .is_empty(),
            "{label}: the child result left no mail to wake the root"
        );
    }
    let mut config = next_host(
        root,
        lost,
        serde_json::json!([{"output": {"text": "continued"}}]),
        Arc::clone(&lost.store),
    );
    // A crash mid-turn drops the driver without its graceful lease release, so
    // the branch stays leased until the lease expires. The idle cuts release
    // on the actor drop; the busy cuts do not, and the holder is this same
    // process, so only the lease expiring lets the reopen take the branch.
    if path == "busy" {
        wait_out_crashed_lease();
    }
    config.resume_after_loss = if expect.continues {
        ResumeAfterLoss::Continue
    } else {
        ResumeAfterLoss::Hold
    };
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    if expect.continues {
        wait_for(
            &events,
            &mut seen,
            &format!("{label}: the continuation"),
            turnish,
        );
        assert_eq!(
            dispatches(&lost.evidence),
            2,
            "{label}: the loss did not continue"
        );
    } else {
        wait_for(
            &events,
            &mut seen,
            &format!("{label}: idle"),
            crate::runtime::root_loss_open_tests::idle,
        );
        assert!(
            !seen.iter().any(turnish),
            "{label}: the stop continued: {seen:#?}"
        );
        assert_eq!(dispatches(&lost.evidence), 1, "{label}: the stop continued");
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    // A loss continuation is a host wake, so it does not clear suppression,
    // and its admission can consume the mail. Probe after it, and only
    // where the cut must refuse the wake: probing the cut that still wakes
    // would admit that wake and end the loss this check just read.
    if !expect.wakes {
        let mut wake_config = next_host(root, lost, serde_json::json!([]), Arc::clone(&lost.store));
        wake_config.resume_after_loss = ResumeAfterLoss::Hold;
        assert!(
            !crate::runtime::host_loop::wake::probe_child_wake(wake_config),
            "{label}: a child result woke the root"
        );
    }
}

/// Waits out the branch lease a crashed running turn left behind.
///
/// The driver's lease release runs only in its graceful shutdown, which a cut
/// skips by panicking the worker thread. The reopen cannot reclaim the branch
/// early: the holder was this same process, and the store treats that as not
/// provably gone. The production lease is 30 s and the last renewal predates
/// the crash, so it has expired by 32 s.
fn wait_out_crashed_lease() {
    std::thread::sleep(Duration::from_secs(32));
}

fn load_tree(
    roots: &crate::agent::ProductStateRoots,
    id: &str,
) -> crate::agent_tree::ProductAgentTree {
    if let Some(runtime) = crate::topology_runtime::lookup(id) {
        return runtime.read(Clone::clone).unwrap();
    }
    let store = roots.tree_store().unwrap();
    crate::agent_tree::ProductAgentTree::reopen(store.load(id).unwrap().unwrap())
}

fn children_stopped(lost: &Lost, running: &str, held: &str) -> bool {
    use crate::agent_tree::InterruptCause;
    use crate::agent_tree_protocol::NodeStatus;
    let tree = load_tree(&lost.state_roots, &lost.id);
    let running_stopped = tree
        .node(running)
        .is_ok_and(|node| node.status == NodeStatus::Interrupted)
        && tree.operations().values().any(|operation| {
            operation.caller_agent_id == running
                && crate::agent_tree::interrupt_cause(operation)
                    .is_ok_and(|cause| cause == Some(Some(InterruptCause::Stopped)))
        });
    running_stopped && tree.loss_continuation(held).is_none()
}

fn turnish(event: &HostEvent) -> bool {
    crate::runtime::recover_tests::turn_completed(event)
}

/// A sleep the session's own registry shows as running. Dropping the guard
/// stops it if the test did not.
struct RunningSleep {
    registry: Arc<crate::jobs::JobRegistry>,
}

impl RunningSleep {
    fn start(registry: Arc<crate::jobs::JobRegistry>, tree: &str, root: &std::path::Path) -> Self {
        use plakous::model::{BranchId, CancellationToken, LineId, RunId, SessionId};
        use plakous::tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, JobOwner,
            JoinedProcessCommand, JoinedProcessShellProfile, JoinedProcessStartRequest,
            JoinedProcessTerminalMode, JoinedSessionsPort, OperationContext,
        };
        use std::str::FromStr;
        let branch = BranchId::from_str("00000000-0000-0000-0000-000000000702").unwrap();
        let owner = JobOwner {
            line: LineId::started_by(branch),
            generation: 1,
            session_id: SessionId::from_str("00000000-0000-0000-0000-000000000701").unwrap(),
            branch_id: branch,
            fence: 1,
        };
        let port = JoinedSessionsPort::local(
            ExecutionEnvironment::host(
                ExecutionEnvironmentScope::Run(
                    RunId::from_str("00000000-0000-0000-0000-000000000704").unwrap(),
                ),
                root,
                ".",
                ExecutionEnvironmentConfig::default(),
            )
            .unwrap()
            .joined_process_sessions()
            .unwrap(),
        );
        let service = registry
            .for_agent("root", tree)
            .attach(owner, port)
            .unwrap();
        let local = service.as_local().unwrap();
        let id = local
            .start(
                JoinedProcessStartRequest::new(
                    JoinedProcessCommand::shell(
                        JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()]).unwrap(),
                        "exec sleep 600",
                    )
                    .unwrap(),
                    ".",
                    Vec::new(),
                    true,
                    JoinedProcessTerminalMode::Pipes,
                )
                .unwrap(),
                OperationContext::new(CancellationToken::new()),
            )
            .unwrap();
        plakous_tools::internal::promote(local, id).unwrap();
        assert!(
            registry.running_count() > 0,
            "the process never became visible"
        );
        Self { registry }
    }

    fn stopping(&self) -> bool {
        self.registry.snapshot().iter().any(|row| {
            matches!(
                row.status,
                plakous_host_protocol::views::ProcessStatus::Stopping
                    | plakous_host_protocol::views::ProcessStatus::Stopped
            )
        })
    }
}

impl Drop for RunningSleep {
    fn drop(&mut self) {
        let _ = self.registry.stop_user(None);
    }
}

/// An idle stop stops a running process before it answers.
#[test]
fn a_stop_of_an_idle_session_stops_its_process() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
    let id = config.session.id.clone();
    let registry = Arc::clone(config.state_roots.job_registry());
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    wait_for(&events, &mut Vec::new(), "the idle session", |event| {
        matches!(event, HostEvent::Status { phase, .. }
            if matches!(phase, plakous_host_protocol::status::HostPhase::Idle))
    });
    let sleep = RunningSleep::start(registry, &id, root.path());
    assert!(
        sleep.registry.snapshot().iter().any(|row| {
            matches!(
                row.status,
                plakous_host_protocol::views::ProcessStatus::Running
            )
        }),
        "the process is not a row the stop can see"
    );
    let ticket = actions.request_stop().unwrap();
    assert_eq!(settled(&actions, ticket), Ok(()));
    assert!(
        sleep.stopping(),
        "the process was still running when the stop was answered"
    );
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
}

/// A running turn's stop starts the process stop before the turn ends, so
/// the idle sweep is not what stops it.
#[test]
fn a_stop_of_a_running_turn_stops_its_process_before_the_turn_ends() {
    let root = tempfile::tempdir().unwrap();
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "stall_ignores_cancellation": true,
            "steps": [{"stream": [{"text": "running"}, {"stall": 30000}]}]
        }))
        .unwrap(),
    );
    let (config, _) = scripted_host_with(root.path(), plan);
    let id = config.session.id.clone();
    let registry = Arc::clone(config.state_roots.job_registry());
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    send_attached_action(&actions, UserAction::Prompt("work".into()));
    wait_for(
        &events,
        &mut Vec::new(),
        "the running turn",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("running")),
    );
    let sleep = RunningSleep::start(registry, &id, root.path());
    let ticket = actions.request_stop().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sleep.stopping() {
        assert!(
            Instant::now() < deadline,
            "the process was not stopped while the turn was still running"
        );
        assert!(
            actions.stop_settled(ticket).is_none(),
            "the stop was answered before its process was stopped"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    actions.request_abandon_and_release();
    let _ = host.join().unwrap();
}

fn crash_at(root: &std::path::Path, lost: &Lost, at: &str) {
    let _guard = crate::runtime::test_seams::Guard;
    crate::runtime::test_seams::arm_stop_cut(&lost.id, at);
    let config = next_host(root, lost, serde_json::json!([]), Arc::clone(&lost.store));
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the held notice", held_notice);
    let _ = actions.request_stop().unwrap();
    assert!(
        host.join().is_err(),
        "{at}: the actor did not stop at the cut"
    );
}

/// The stop answer arrives from the settle's wake, with the notice emitted
/// first and a delay before the settle, and no later frame.
#[test]
fn a_delayed_settle_still_wakes_the_waiting_connection() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([{"output": {"text": "stored"}}])),
    );
    let id = config.session.id.clone();
    let _guard = crate::runtime::test_seams::Guard;
    crate::runtime::test_seams::arm_notice_delay(&id, Duration::from_millis(300));
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    send_attached_action(&actions, UserAction::Prompt("store it".into()));
    wait_for(
        &events,
        &mut Vec::new(),
        "the storing turn",
        crate::runtime::recover_tests::turn_completed,
    );
    let session = WireSession::start(
        NodeAddress {
            tree_id: id.clone(),
            agent_id: id.clone(),
        },
        id,
        actions.clone(),
        events,
        |_| None,
    );
    let output = Output::default();
    let mut connection = session.connect(output.clone(), Face::Remote, || false);
    let frame = ClientFrame::<TestExtension>::Request {
        id: 7,
        op: Op::StopSession {
            session: session.session_id(),
        },
    };
    assert!(connection.frame(Ok(encode_line(&frame).unwrap())).unwrap());
    let mut changes = session.changes();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let answer = runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_a_wake_without_the_answer = false;
        loop {
            tokio::time::timeout_at(deadline, changes.changed())
                .await
                .expect("the settle never woke the connection")
                .unwrap();
            connection.flush().unwrap();
            let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
            if let Some(response) = text
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .find(|frame| frame["frame"] == "response" && frame["id"] == 7)
            {
                assert!(
                    saw_a_wake_without_the_answer,
                    "the answer arrived with the notice, before the settle woke"
                );
                return response["outcome"].clone();
            }
            saw_a_wake_without_the_answer = true;
        }
    });
    assert_eq!(
        answer,
        serde_json::json!({"ok": {"answer": "stopped", "runner_pending": false}})
    );
    connection.leave();
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
}

/// The residency word is the one read before the receive. An attach, a
/// prompt and a detach in the gap still run the prompt.
#[test]
fn an_attach_during_the_idle_decision_still_runs_its_prompt() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([{"output": {"text": "ran"}}])),
    );
    let attachments = AttachmentCount::default();
    config.idle_release = Some(IdleRelease {
        after: Duration::ZERO,
        attachments: attachments.clone(),
    });
    let _guard = crate::runtime::test_seams::Guard;
    crate::runtime::test_seams::arm_idle_release(&config.session.id);
    let (actions, events, host) = spawn_host_worker(config).unwrap();
    crate::runtime::test_seams::wait_until_idle_seam();
    let hold = attachments.hold().expect("the session took a hold");
    send_attached_action(&actions, UserAction::Prompt("run this".into()));
    drop(hold);
    crate::runtime::test_seams::release_idle_seam();
    let mut seen = Vec::new();
    wait_for(
        &events,
        &mut seen,
        "the prompt",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("ran")),
    );
    send_attached_action(&actions, UserAction::Quit);
    host.join().unwrap().unwrap();
}
