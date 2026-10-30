#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
use plakous_host_protocol::status::HostPause;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;
use crate::runtime::test_kit::ProbeIntent;

use super::*;
use crate::runtime::*;

use crate::runtime::test_kit::MemoryRows;
use crate::sessions::{
    QueueDrainState as SessionQueueDrainState, QueueItem as SessionQueueItem, SessionRecord,
};
use std::path::PathBuf;

#[test]
fn queue_store_failure_classification_is_an_exact_allowlist() {
    for (reason, class) in [
        ("product session changed concurrently", "concurrent_change"),
        (
            "product session revision is exhausted",
            "revision_exhausted",
        ),
        (
            "product session identity already exists or is invalid",
            "identity_conflict",
        ),
        (
            "queue identity contradicts durable product state",
            "queue_identity_conflict",
        ),
        (
            "queue tombstone contradicts durable state",
            "queue_identity_conflict",
        ),
    ] {
        assert_eq!(queue_store_failure_class(reason), class);
        assert_eq!(
            queue_store_failure_class(&format!("{reason}: private details")),
            "store_failure"
        );
    }
    assert_eq!(
        queue_store_failure_class("private arbitrary backend error"),
        "store_failure"
    );
}

#[test]
fn editable_prompt_queue_has_explicit_count_and_size_bounds() {
    let mut queue = (0..MAX_PENDING_PROMPTS)
        .map(|index| SessionQueueItem::new(format!("prompt {index}")))
        .collect::<Vec<_>>();
    assert_eq!(
        queue::enqueue(&mut queue, "overflow"),
        Err(queue::Refusal::Full)
    );
    assert_eq!(queue.len(), MAX_PENDING_PROMPTS);

    queue.clear();
    assert_eq!(
        queue::enqueue(&mut queue, "x".repeat(MAX_PROMPT_BYTES + 1)),
        Err(queue::Refusal::TooLarge)
    );
    assert_eq!(queue, [] as [queue::QueueItem; 0]);
}

#[test]
fn recovery_blocked_policy_refuses_binding_workflow_and_queue_mutations() {
    let blocked = [
        UserAction::Prompt("fresh work".into()),
        UserAction::ForkSession {
            account: None,
            compact: false,
        },
        UserAction::Compact,
        UserAction::ResumeQueueDrain,
        UserAction::InvokeTool {
            correlation_id: None,
            name: "list_agents".into(),
            arguments: "{}".into(),
            agent_id: None,
        },
    ];
    assert!(blocked.iter().all(recovery_blocked_action));
    assert!(!recovery_blocked_action(&UserAction::ClearSession));
    assert!(!recovery_blocked_action(&UserAction::Extension(
        ProbeIntent::InspectUsage
    )));
    assert!(!recovery_blocked_action(&UserAction::Quit));
}

#[test]
fn idle_immediate_prompt_emits_user_message_without_one_item_queue_chip() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    let item = queue::enqueue(&mut session.pending_queue, "hello world").unwrap();
    let submitted_at_ms = item.submitted_at_ms;
    assert!(
        submitted_at_ms > 0,
        "enqueue stamps the operator's submit time"
    );
    session.record_prompt(item.text.clone());
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let (text, queued, echoed) = admit_idle_immediate_prompt(
        &events,
        &session,
        UserAction::Prompt("hello world".into()),
        item,
        true,
    );
    assert_eq!(text, "hello world");
    assert!(queued.is_some());
    assert!(echoed);
    let observed = rx.try_iter().collect::<Vec<_>>();
    assert!(
        !observed
            .iter()
            .any(|event| matches!(event, HostEvent::QueueSnapshot { .. })),
        "immediate drain must not flash a one-item queue chip: {observed:?}"
    );
    assert!(
        observed.iter().any(
            |event| matches!(event, HostEvent::UserMessage { text, sent_at_ms } if text == "hello world" && *sent_at_ms == Some(submitted_at_ms))
        ),
        "idle admission must echo UserMessage immediately: {observed:?}"
    );
}

#[test]
fn idle_prompt_with_existing_queue_projects_remaining_chip() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    let _first = queue::enqueue(&mut session.pending_queue, "older").unwrap();
    let item = queue::enqueue(&mut session.pending_queue, "newer").unwrap();
    session.record_prompt(item.text.clone());
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let _ = admit_idle_immediate_prompt(
        &events,
        &session,
        UserAction::Prompt("newer".into()),
        item,
        true,
    );
    let observed = rx.try_iter().collect::<Vec<_>>();
    assert!(
        observed.iter().any(|event| matches!(
            event,
            HostEvent::QueueSnapshot { items, .. } if items.len() == 2
        )),
        "remaining FIFO is real chrome: {observed:?}"
    );
    assert!(
        observed
            .iter()
            .any(|event| matches!(event, HostEvent::UserMessage { text, .. } if text == "newer"))
    );
}

#[test]
fn queue_fifo_and_drop_last() {
    let mut q = Vec::new();
    let _ = queue::enqueue(&mut q, "a");
    let _ = queue::enqueue(&mut q, "b");
    assert_eq!(queue::enqueue(&mut q, "  "), Err(queue::Refusal::Empty));
    assert_eq!(
        queue::take_next(&mut q).map(|item| item.text),
        Some("a".into())
    );
    assert_eq!(
        queue::drop_last(&mut q).map(|item| item.text),
        Some("b".into())
    );
    assert!(queue::take_next(&mut q).is_none());
}

#[test]
fn prompt_history_is_current_session_adjacent_deduped_and_newest_500() {
    let mut session = SessionRecord::new(PathBuf::from("/work/history"));
    session.transcript.append(TranscriptRow::user("discarded"));
    session.transcript.append(TranscriptRow::user("discarded"));
    for index in 0..501 {
        session.record_prompt(format!("prompt-{index}"));
    }
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    emit_prompt_history(&events, &session);
    let snapshot = rx.try_iter().find_map(|event| match event {
        HostEvent::PromptHistorySnapshot(prompts) => Some(prompts),
        _ => None,
    });
    let Some(prompts) = snapshot else {
        panic!("prompt history snapshot was not emitted");
    };
    assert_eq!(prompts.len(), 500);
    assert_eq!(prompts.first().map(String::as_str), Some("prompt-1"));
    assert_eq!(prompts.last().map(String::as_str), Some("prompt-500"));
    assert!(!prompts.iter().any(|prompt| prompt == "discarded"));
}

#[test]
fn exact_resume_snapshot_uses_only_durable_session_prompt_history() {
    let root = tempfile::tempdir().unwrap();
    let paths = MemoryRows::new();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session
        .transcript
        .append(TranscriptRow::user("transcript-is-not-history"));
    session.record_prompt("durable prompt".into());
    let id = session.id.clone();
    {
        use crate::session_store::SessionRecordStore as _;
        paths.records().save(&mut session).unwrap();
    }
    let resumed = {
        use crate::session_store::SessionRecordStore as _;
        paths.records().load(&id).unwrap()
    };
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    emit_prompt_history(&events, &resumed);
    assert!(rx.try_iter().any(|event| matches!(
        event,
        HostEvent::PromptHistorySnapshot(prompts) if prompts == ["durable prompt"]
    )));
}

#[test]
fn queue_mutation_targets_stable_identity_with_duplicate_safe_text() {
    let mut q = Vec::new();
    let _ = queue::enqueue(&mut q, "first");
    let _ = queue::enqueue(&mut q, "second");
    let _ = queue::enqueue(&mut q, "second");
    let target = q[1].clone();
    assert_eq!(
        queue::remove(&mut q, &target.id, target.revision).map(|item| item.text),
        Some("second".into())
    );
    assert_eq!(
        queue::take_next(&mut q).map(|item| item.text),
        Some("first".into())
    );
    assert_eq!(
        queue::take_next(&mut q).map(|item| item.text),
        Some("second".into())
    );
    assert!(queue::take_next(&mut q).is_none());
}

#[test]
fn unresolved_delivery_locks_every_queue_mutation() {
    let mut session = SessionRecord::new(PathBuf::from("/work/locked"));
    let item = SessionQueueItem::new("immutable retry");
    session.pending_queue.push(item.clone());
    let intents = sessions::projector::ProductIntents {
        submissions: vec![sessions::projector::Submission {
            command_id: "plak.command.locked".into(),
            kind: sessions::projector::SubmissionKind::RootStart,
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: item.text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        }],
        ..Default::default()
    };
    session.queue_commands = sessions::QueueCommands::from_join(&intents, None);
    assert!(!drop_last_mutable(&mut session));
    assert!(!remove_mutable(&mut session, &item.id, item.revision));
    assert!(edit_mutable(&mut session, &item.id, item.revision, "changed").is_err());
    assert!(!move_mutable(&mut session, &item.id, item.revision, 0));
    assert_eq!(session.pending_queue, vec![item]);
    assert!(session.validate_for_store().is_ok());
}

/// A paused tree or a paused
/// queue must reach the HUD, and attach must say what the
/// state is and exactly how to leave it.
#[test]
fn a_paused_queue_is_named_in_the_status_row_and_in_an_attach_note() {
    let root = tempfile::tempdir().unwrap();
    let paths = MemoryRows::new();
    let mut session = SessionRecord::new(root.path().to_path_buf());

    assert_eq!(
        restored_paused_status_suffix(&paths, &session),
        HostPause::default()
    );
    // A note claims only what the host observed: a tree the host cannot
    // read is not reported paused (a contended hub lock would otherwise make
    // every fresh attach announce a paused tree).
    assert!(!topology_state::observed_restored_paused(
        &paths,
        "no-such-tree"
    ));

    // A restored drain state with nothing queued is not a paused
    // queue; a childless reopen says nothing about draining.
    session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
    assert_eq!(
        restored_paused_status_suffix(&paths, &session),
        HostPause::default()
    );
    session.pending_queue.push(SessionQueueItem::new("later"));
    assert_eq!(
        restored_paused_status_suffix(&paths, &session),
        HostPause {
            tree: false,
            queue: true
        }
    );

    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    emit_restored_paused_note(&events, &paths, &session, false);
    assert!(
        events.take_notice_intents().is_empty(),
        "every attach re-announces the note, so it is never stored"
    );
    drop(events);
    let pause = rx
        .try_iter()
        .find_map(|event| match event {
            HostEvent::RestoredPause(pause) => Some(pause),
            _ => None,
        })
        .expect("attach names the paused state");
    assert!(pause.queue);
    assert!(!pause.tree);
}

/// Every operator-facing string for the paused state is prose and
/// names a remedy. `ResumeTree` and `restored_paused` are wire values.
#[test]
fn paused_tree_copy_is_prose_with_a_remedy() {
    for copy in [
        crate::agent_tree::ProductAgentTree::TREE_PAUSED_REFUSAL,
        crate::agent_tree::ProductAgentTree::TREE_EPOCH_REBIND_PAUSED,
        crate::agent::RESTORED_PAUSED_REFUSAL,
    ] {
        assert!(
            !copy.contains("ResumeTree") && !copy.contains("restored_paused"),
            "raw enum variant in operator copy: {copy}"
        );
        assert!(
            copy.contains("paused"),
            "the copy must name the state: {copy}"
        );
        assert!(
            copy.contains("resume") || copy.contains("--resume-tree"),
            "the copy must name a remedy: {copy}"
        );
    }
}
