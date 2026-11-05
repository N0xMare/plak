//! A host releases an idle session (spec 02 §4.4): once nothing keeps it
//! resident for the idle release, the session's own actor quits joined, and
//! an attachment keeps it resident until it detaches. Every wait here is for
//! an event or the worker's end, bounded; the short idle release is the
//! test's own deadline, and nothing asserts on elapsed time but "still
//! resident".

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::NodeAddress;

use crate::host_service::{AttachmentCount, HostService};
use crate::runtime::recover_tests::{scripted_host_with, wait_for};
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
    let woken = slot.resident().unwrap();
    assert_eq!(*asked.lock().unwrap(), [first]);
    assert!(!woken.ended());
    slot.quit(Duration::from_secs(30)).unwrap();
}
