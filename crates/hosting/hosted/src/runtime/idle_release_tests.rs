//! A host releases an idle session (spec 02 §4.4): once nothing keeps it
//! resident for the idle release, the session's own actor quits joined; an
//! attachment or a running process keeps it resident until it ends, and a
//! trigger after the release began is served the session opened again.
//! Every wait here is for an event or the worker's end, bounded; the short
//! idle release is the test's own deadline, and nothing asserts on elapsed
//! time but "still resident".

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::NodeAddress;

use crate::host_service::{AttachmentCount, HostService};
use crate::runtime::recover_tests::{scripted_host_with, turn_completed, wait_for};
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::send_attached_action;
use crate::runtime::{IdleRelease, spawn_host_worker};

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

fn answered_once() -> Arc<plakous_offline_crash_harness::script::ScriptPlan> {
    Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "steps": [{"output": {"text": "the idle answer"}}]
        }))
        .unwrap(),
    )
}

fn ended(worker: &crate::runtime::HostWorkerHandle, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if worker.is_finished() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    worker.is_finished()
}

/// A settled session with an attachment stays resident past the idle
/// release; once the attachment detaches, the session releases itself with
/// no quit sent.
#[test]
fn an_idle_session_releases_once_its_last_attachment_detaches() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = scripted_host_with(root.path(), answered_once());
    let attachments = AttachmentCount::default();
    config.idle_release = Some(IdleRelease {
        after: Duration::from_millis(200),
        attachments: attachments.clone(),
    });
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let node = NodeAddress {
        tree_id: "idle".into(),
        agent_id: "idle".into(),
    };
    let service =
        HostService::new(actions.clone(), node.clone(), 64).with_attachment_count(&attachments);
    let attachment = service
        .attach(&node, crate::host_service::wire::Face::Local)
        .unwrap();
    assert_eq!(attachments.get(), 1);
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("hello".into()));
    wait_for(&events, &mut seen, "the turn settled", |event| {
        matches!(event, HostEvent::SetBusy(false))
    });
    assert!(
        !ended(&worker, Duration::from_secs(1)),
        "an attached session was released"
    );
    service.detach(attachment).unwrap();
    assert_eq!(attachments.get(), 0);
    drop(service);
    assert!(
        ended(&worker, Duration::from_secs(30)),
        "the idle session was not released"
    );
    drop(actions);
    worker.join().unwrap().unwrap();
}

/// An embedded session (no idle release) is never released for being idle.
#[test]
fn an_embedded_session_is_never_released_for_being_idle() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(root.path(), answered_once());
    assert!(config.idle_release.is_none());
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("hello".into()));
    wait_for(&events, &mut seen, "the turn settled", |event| {
        matches!(event, HostEvent::SetBusy(false))
    });
    assert!(!ended(&worker, Duration::from_secs(1)));
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

/// A host's slot serves a session until it releases itself, then holds it
/// dormant, and the next trigger opens it again by the id it served: a
/// release, a wake and a second release, with the opener asked once per
/// wake for exactly that session.
#[test]
fn a_slot_wakes_a_released_session_by_its_id() {
    use crate::host_service::wire::{OpenedSession, SessionSlot};
    let root = tempfile::tempdir().unwrap();
    let open = {
        let root = root.path().to_path_buf();
        move |name: &str| -> crate::CliResult<OpenedSession<TestExtension>> {
            std::fs::create_dir_all(root.join(name)).unwrap();
            let (mut config, _) = scripted_host_with(&root.join(name), answered_once());
            let attachments = AttachmentCount::default();
            config.idle_release = Some(IdleRelease {
                after: Duration::from_millis(200),
                attachments: attachments.clone(),
            });
            let session_id = config.session.id.clone();
            let node = NodeAddress {
                tree_id: session_id.clone(),
                agent_id: session_id.clone(),
            };
            let (actions, events, worker) = spawn_host_worker(config)?;
            Ok(OpenedSession {
                root: node,
                session_id,
                actions,
                events,
                worker,
                attachments,
                roots: Box::new(|_| None),
            })
        }
    };
    let opened = open("first").unwrap();
    let first = opened.session_id.clone();
    let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let slot = SessionSlot::new(opened, {
        let (asked, open) = (Arc::clone(&asked), open.clone());
        move |id: &str| {
            asked.lock().unwrap().push(id.to_owned());
            open("again")
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while !slot.dormant() {
        assert!(
            Instant::now() < deadline,
            "the idle session was not released"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let (woken, _hold) = slot.resident().unwrap();
    assert_eq!(*asked.lock().unwrap(), [first]);
    assert!(!woken.ended());
    slot.quit(Duration::from_secs(30)).unwrap();
}

/// A session opened for a slot, with an idle release too long to come
/// due in a test: what it releases is the test's to decide.
fn slot_session(
    root: &std::path::Path,
    name: &str,
) -> crate::CliResult<crate::host_service::wire::OpenedSession<TestExtension>> {
    std::fs::create_dir_all(root.join(name)).unwrap();
    let (mut config, _) = scripted_host_with(&root.join(name), answered_once());
    let attachments = AttachmentCount::default();
    config.idle_release = Some(IdleRelease {
        after: Duration::from_secs(3600),
        attachments: attachments.clone(),
    });
    let session_id = config.session.id.clone();
    let node = NodeAddress {
        tree_id: session_id.clone(),
        agent_id: session_id.clone(),
    };
    let (actions, events, worker) = spawn_host_worker(config)?;
    Ok(crate::host_service::wire::OpenedSession {
        root: node,
        session_id,
        actions,
        events,
        worker,
        attachments,
        roots: Box::new(|_| None),
    })
}

/// The race between the timer and an attach, at the step that decides it:
/// a hold or an attachment taken first keeps the release from beginning,
/// and once it began, the releasing session takes neither.
#[test]
fn a_release_begins_only_with_nothing_held_and_then_takes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let opened = slot_session(root.path(), "race").unwrap();
    let count = opened.attachments.clone();
    let service = HostService::new(opened.actions.clone(), opened.root.clone(), 64)
        .with_attachment_count(&count);
    let hold = count.hold().unwrap();
    assert!(
        !count.begin_release(count.seen()),
        "a release began over a hold"
    );
    drop(hold);
    let attachment = service
        .attach(&opened.root, crate::host_service::wire::Face::Remote)
        .unwrap();
    assert!(
        !count.begin_release(count.seen()),
        "a release began over an attachment"
    );
    service.detach(attachment).unwrap();
    let seen = count.seen();
    drop(count.hold().unwrap());
    assert!(
        !count.begin_release(seen),
        "a release began over a hold taken and dropped since the read"
    );
    assert!(count.begin_release(count.seen()));
    assert_eq!(
        service.attach(&opened.root, crate::host_service::wire::Face::Remote),
        Err(plakous_host_protocol::wire::RequestRefusal::NodeRetiring)
    );
    assert!(count.hold().is_none());
    assert_eq!(count.get(), 0);
    send_attached_action(&opened.actions, UserAction::Quit);
    opened.worker.join().unwrap().unwrap();
}

/// A trigger that comes after the session began to release is never handed
/// the releasing session: the slot waits for the release to commit, then
/// opens the session again by its id and hands the trigger that one.
#[test]
fn a_trigger_during_a_release_gets_the_reopened_session() {
    use crate::host_service::wire::SessionSlot;
    let root = tempfile::tempdir().unwrap();
    let opened = slot_session(root.path(), "first").unwrap();
    let first = opened.session_id.clone();
    let releasing = opened.attachments.clone();
    let quit = opened.actions.clone();
    let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let slot = SessionSlot::new(opened, {
        let (asked, root) = (Arc::clone(&asked), root.path().to_path_buf());
        move |id: &str| {
            asked.lock().unwrap().push(id.to_owned());
            slot_session(&root, "again")
        }
    });
    // The actor's decision, then its joined quit a moment later.
    assert!(releasing.begin_release(releasing.seen()));
    assert!(slot.dormant(), "a releasing session is not served");
    let quitter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        send_attached_action(&quit, UserAction::Quit);
    });
    let (woken, _hold) = slot.resident().unwrap();
    quitter.join().unwrap();
    assert_eq!(*asked.lock().unwrap(), [first.as_str()]);
    assert_ne!(
        woken.session_id(),
        first,
        "the releasing session was handed out"
    );
    assert!(!woken.ended());
    assert!(!slot.dormant());
    slot.quit(Duration::from_secs(30)).unwrap();
}

/// A background process keeps its session resident past the idle release;
/// once it ends the session releases itself.
#[test]
fn a_running_background_process_keeps_the_session_until_it_ends() {
    let root = tempfile::tempdir().unwrap();
    let plan: Arc<plakous_offline_crash_harness::script::ScriptPlan> = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "process_tools": true,
            "repeat_last_step": true,
            "steps": [
                {"output": {"tool": {"name": "process_session_start", "arguments": {
                    "command": "for i in $(seq 600); do [ -e release-job ] && exit 0; sleep 0.05; done",
                    "workdir": ".", "login": false, "tty": false,
                    "keep_stdin_open": true, "yield_time_ms": 0, "max_output_bytes": 4096
                }}}, "finish": "tool_calls"},
                {"output": {"text": "the process runs on"}}
            ]
        }))
        .unwrap(),
    );
    let (mut config, _) = scripted_host_with(root.path(), plan);
    config.idle_release = Some(IdleRelease {
        after: Duration::from_millis(200),
        attachments: AttachmentCount::default(),
    });
    let workspace = config.cwd.clone();
    let jobs = Arc::clone(config.state_roots.job_registry());
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("start it".into()));
    wait_for(&events, &mut seen, "the turn completed", turn_completed);
    assert_eq!(jobs.running_count(), 1, "the process runs past its turn");
    assert!(
        !ended(&worker, Duration::from_secs(1)),
        "a session with a running process was released"
    );
    std::fs::write(workspace.join("release-job"), b"").unwrap();
    assert!(
        ended(&worker, Duration::from_secs(30)),
        "the session was not released once its process ended"
    );
    drop(actions);
    worker.join().unwrap().unwrap();
}

type Mailbox = crate::surface_protocol::UserActionSender<TestExtension>;

/// A session whose actor releases it at its first decision once idle, with
/// the hooks `arm` sets run around that decision's mark: its count,
/// mailbox, events and worker. `arm` runs before the worker starts, and
/// its hooks receive the mailbox from `handed` once the worker has one.
fn released_at_once(
    root: &std::path::Path,
    arm: impl FnOnce(&AttachmentCount, std::sync::mpsc::Receiver<Mailbox>),
) -> (
    AttachmentCount,
    Mailbox,
    crate::surface_protocol::HostEventReceiver<TestExtension>,
    crate::runtime::HostWorkerHandle,
) {
    let (mut config, _) = scripted_host_with(root, answered_once());
    let attachments = AttachmentCount::default();
    config.idle_release = Some(IdleRelease {
        after: Duration::ZERO,
        attachments: attachments.clone(),
    });
    let (hand, handed) = std::sync::mpsc::channel();
    arm(&attachments, handed);
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    hand.send(actions.clone()).unwrap();
    (attachments, actions, events, worker)
}

fn idle_node() -> NodeAddress {
    NodeAddress {
        tree_id: "idle".into(),
        agent_id: "idle".into(),
    }
}

/// A client that attaches, sends a prompt and detaches between the
/// actor's read of the count and its mark leaves nothing counted at the
/// mark, but the release does not begin: the prompt it sent was accepted,
/// so it runs, and the session releases at a later decision.
#[test]
fn an_intent_from_a_connection_gone_before_the_mark_still_runs() {
    let root = tempfile::tempdir().unwrap();
    let (marked, first_mark) = std::sync::mpsc::channel();
    let (attachments, actions, events, worker) =
        released_at_once(root.path(), move |attachments, handed| {
            let count = attachments.clone();
            attachments.around_next_mark(
                move || {
                    let actions = handed.recv().unwrap();
                    let service = HostService::new(actions.clone(), idle_node(), 64)
                        .with_attachment_count(&count);
                    let attachment = service
                        .attach(&idle_node(), crate::host_service::wire::Face::Remote)
                        .unwrap();
                    send_attached_action(&actions, UserAction::Prompt("hello".into()));
                    service.detach(attachment).unwrap();
                    assert_eq!(count.get(), 0);
                },
                move |was_marked| {
                    let _ = marked.send(was_marked);
                },
            );
        });
    assert!(
        !first_mark
            .recv_timeout(Duration::from_secs(30))
            .expect("the actor decided"),
        "the release began over the prompt"
    );
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "the prompt's turn", turn_completed);
    assert!(
        ended(&worker, Duration::from_secs(30)),
        "the idle session was not released after its turn"
    );
    assert!(attachments.releasing());
    drop(actions);
    worker.join().unwrap().unwrap();
}

/// An attach between the actor's read of the count and its mark keeps the
/// session resident until it detaches.
#[test]
fn an_attach_before_the_actors_mark_keeps_the_session() {
    let root = tempfile::tempdir().unwrap();
    let (marked, first_mark) = std::sync::mpsc::channel();
    let held = Arc::new(std::sync::Mutex::new(None));
    let (attachments, actions, _events, worker) = released_at_once(root.path(), {
        let held = Arc::clone(&held);
        move |attachments, handed| {
            let count = attachments.clone();
            attachments.around_next_mark(
                move || {
                    let service = HostService::new(handed.recv().unwrap(), idle_node(), 64)
                        .with_attachment_count(&count);
                    let attachment = service
                        .attach(&idle_node(), crate::host_service::wire::Face::Remote)
                        .unwrap();
                    *held.lock().unwrap() = Some((service, attachment));
                },
                move |was_marked| {
                    let _ = marked.send(was_marked);
                },
            );
        }
    });
    assert!(
        !first_mark
            .recv_timeout(Duration::from_secs(30))
            .expect("the actor decided"),
        "the release began over an attachment"
    );
    assert!(
        !ended(&worker, Duration::from_secs(1)),
        "an attached session was released"
    );
    assert!(!attachments.releasing());
    let (service, attachment) = held.lock().unwrap().take().unwrap();
    service.detach(attachment).unwrap();
    drop(service);
    assert!(
        ended(&worker, Duration::from_secs(30)),
        "the session was not released once its attachment detached"
    );
    drop(actions);
    worker.join().unwrap().unwrap();
}

/// An attach after the actor's mark is refused `node_retiring`, takes no
/// hold, and the session releases.
#[test]
fn an_attach_after_the_actors_mark_is_refused_and_the_session_releases() {
    let root = tempfile::tempdir().unwrap();
    let (answered, after_mark) = std::sync::mpsc::channel();
    let (attachments, actions, _events, worker) =
        released_at_once(root.path(), move |attachments, handed| {
            let count = attachments.clone();
            attachments.around_next_mark(
                || {},
                move |was_marked| {
                    let service = HostService::new(handed.recv().unwrap(), idle_node(), 64)
                        .with_attachment_count(&count);
                    let attach =
                        service.attach(&idle_node(), crate::host_service::wire::Face::Remote);
                    let _ = answered.send((was_marked, attach.err(), count.hold().is_none()));
                },
            );
        });
    let (was_marked, refusal, no_hold) = after_mark
        .recv_timeout(Duration::from_secs(30))
        .expect("the actor decided");
    assert!(was_marked);
    assert_eq!(
        refusal,
        Some(plakous_host_protocol::wire::RequestRefusal::NodeRetiring)
    );
    assert!(no_hold, "a releasing session took a hold");
    assert!(
        ended(&worker, Duration::from_secs(30)),
        "the marked session was not released"
    );
    assert_eq!(attachments.get(), 0);
    drop(actions);
    worker.join().unwrap().unwrap();
}

/// A release that does not join within the slot's budget fails the trigger
/// that waited for it rather than spinning on the releasing session, and
/// leaves the slot dormant with the old worker running. The next trigger's
/// open of the session by id is refused while that worker holds the
/// session's lease, so there are never two of it; once the worker ends, a
/// trigger opens the session again.
#[test]
fn a_release_that_does_not_join_in_time_leaves_no_second_session() {
    use crate::host_service::wire::{OpenedSession, SessionSlot};
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("slow");
    std::fs::create_dir_all(&workspace).unwrap();
    let (mut config, _) = scripted_host_with(&workspace, answered_once());
    let attachments = AttachmentCount::default();
    config.idle_release = Some(IdleRelease {
        after: Duration::from_secs(3600),
        attachments: attachments.clone(),
    });
    let session_id = config.session.id.clone();
    let store = Arc::clone(&config.store);
    let state_roots = config.state_roots.clone();
    let root_node = NodeAddress {
        tree_id: session_id.clone(),
        agent_id: session_id.clone(),
    };
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("store me".into()));
    wait_for(&events, &mut seen, "the stored turn", turn_completed);
    let reopen = reopener(
        workspace.clone(),
        store,
        state_roots,
        Duration::from_secs(3600),
    );
    let slot = Arc::new(
        SessionSlot::new(
            OpenedSession {
                root: root_node,
                session_id: session_id.clone(),
                actions: actions.clone(),
                events,
                worker,
                attachments: attachments.clone(),
                roots: Box::new(|_| None),
            },
            reopen,
        )
        .with_release_join(Duration::from_millis(300)),
    );
    // A release that has begun and does not end: its worker never quits.
    assert!(attachments.begin_release(attachments.seen()));
    let waited = {
        let slot = Arc::clone(&slot);
        let (done, answer) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(slot.resident().map(|_| ()).err());
        });
        answer
            .recv_timeout(Duration::from_secs(30))
            .expect("the trigger waited out the release's join, not spun on the session")
    };
    assert!(waited.is_some(), "a trigger was served a releasing session");
    assert!(slot.dormant());
    let refused = slot
        .resident()
        .map(|_| ())
        .expect_err("a second session opened while the first one's worker runs");
    assert!(
        refused.message().contains("lease is held"),
        "{}",
        refused.message()
    );
    send_attached_action(&actions, UserAction::Quit);
    drop(actions);
    let deadline = Instant::now() + Duration::from_secs(60);
    let woken = loop {
        match slot.resident() {
            Ok((woken, _hold)) => break woken,
            Err(failure) => {
                assert!(
                    Instant::now() < deadline,
                    "the session did not open once its old worker ended: {}",
                    failure.message()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    assert_eq!(woken.session_id(), session_id);
    slot.quit(Duration::from_secs(30)).unwrap();
}

/// Opens the session of an id again from `store` and `state_roots`, as a
/// host's slot does after a release, released once idle for `after`.
pub(super) fn reopener(
    workspace: std::path::PathBuf,
    store: Arc<dyn crate::session_store::SessionRecordStore>,
    state_roots: crate::agent::ProductStateRoots,
    after: Duration,
) -> impl Fn(&str) -> crate::CliResult<crate::host_service::wire::OpenedSession<TestExtension>>
+ Send
+ Sync
+ 'static {
    move |id: &str| {
        let (mut config, _) = scripted_host_with(&workspace, answered_once());
        config.state_roots = state_roots.clone();
        config.store = Arc::clone(&store);
        config.session = store.load(id).map_err(crate::error::CliFailure::new)?;
        let attachments = AttachmentCount::default();
        config.idle_release = Some(IdleRelease {
            after,
            attachments: attachments.clone(),
        });
        let root = NodeAddress {
            tree_id: id.to_owned(),
            agent_id: id.to_owned(),
        };
        let (actions, events, worker) = spawn_host_worker(config)?;
        Ok(crate::host_service::wire::OpenedSession {
            root,
            session_id: id.to_owned(),
            actions,
            events,
            worker,
            attachments,
            roots: Box::new(|_| None),
        })
    }
}
