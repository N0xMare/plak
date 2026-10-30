#![allow(
    clippy::too_many_lines,
    reason = "The idle queue-save failure scenario verifies safe logging and preservation of both queued input and the draft."
)]

use super::*;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::fresh_injected_attached_host;
use crate::session_log;
use crate::surface_protocol::HostEvent;
use std::sync::Arc;

#[test]
fn queue_save_failure_logs_safe_classification_and_preserves_queue_and_draft() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.session.enqueue_live("private older prompt").unwrap();
    config.store.save(&mut config.session).unwrap();
    let durable_store = Arc::clone(&config.store);
    let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(
        config,
        &action_rx,
        crate::runtime::Sender::over(event_tx),
        None,
    )
    .unwrap();
    let before = host.config.session.clone();
    let stored_before = durable_store.load(&before.id).unwrap();
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    for (reason, class) in [
        ("product session changed concurrently", "concurrent_change"),
        (
            "product session changed concurrently: private backend path /secret/token",
            "store_failure",
        ),
    ] {
        host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::refusing(reason));
        let draft = "private unsaved draft";
        actions.send(UserAction::Prompt(draft.into())).unwrap();
        assert!(matches!(
            host.next_prompt(None).unwrap(),
            HostStep::Continue
        ));
        assert_eq!(host.config.session, before);
        assert_eq!(durable_store.load(&before.id).unwrap(), stored_before);
        let observed = event_rx.try_iter().collect::<Vec<_>>();
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::QueueSnapshot { items, .. }
                if items.len() == 1 && items[0].text == "private older prompt"
        )));
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::DraftRejected(UserAction::Prompt(text)) if *text == draft
        )));
        assert!(!observed.iter().any(|event| matches!(
            event,
            HostEvent::DraftAccepted(_) | HostEvent::PromptAdmitted(_)
        )));

        let records = session_log::read_tail_for_kind(
            &host.config.home,
            &before.id,
            10,
            Some("queue.persist_failed"),
        )
        .unwrap();
        let raw = records.last().unwrap();
        let event: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(event["kind"], "queue.persist_failed");
        assert_eq!(event["action"], "admission");
        assert_eq!(event["failure_class"], class);
        assert_eq!(event["attempted_store_revision"], before.store_revision);
        assert_eq!(event.as_object().unwrap().len(), 6);
        assert!(!raw.contains("private"));
        assert!(!raw.contains("/secret/token"));
    }

    // A stale queue action is a refusal, not a failed store write.
    actions
        .send(UserAction::DeleteQueued {
            id: "missing".into(),
            revision: 1,
        })
        .unwrap();
    assert!(matches!(
        host.next_prompt(None).unwrap(),
        HostStep::Continue
    ));
    let failures = session_log::read_tail_for_kind(
        &host.config.home,
        &before.id,
        10,
        Some("queue.persist_failed"),
    )
    .unwrap();
    assert_eq!(failures.len(), 2, "one diagnostic per failed action");
    host.config.store = durable_store;
    actions.send(UserAction::DropQueued).unwrap();
    assert!(matches!(
        host.next_prompt(None).unwrap(),
        HostStep::Continue
    ));
    assert_eq!(
        host.config.session.pending_queue,
        [] as [queue::QueueItem; 0]
    );
    let after_success = session_log::read_tail_for_kind(
        &host.config.home,
        &before.id,
        10,
        Some("queue.persist_failed"),
    )
    .unwrap();
    assert_eq!(after_success, failures, "successful saves are not failures");
    host.release().unwrap();
}
