#![allow(clippy::unwrap_used, reason = "test assertions")]

use super::*;
use crate::agent_tree::{ProductAgentTree, TreeStore as _};
use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, WriterMode};

#[test]
fn child_ledger_prechange_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut tree, child) = root_with_child(dir.path(), "ledger-bytes");
    tree.send_agent_message("root", &parent_mail(&child, "standing instruction"), None).unwrap();
    let driver = TreeDriver::new(
        std::sync::Arc::new(std::sync::Mutex::new(tree.clone())),
        std::sync::Arc::new(crate::agent_tree::MemoryTreeStore::new()),
        tree.vcs_port(), None,
    );
    let id = format!("plak:park-wake:{child}:1:attempt:2");
    let prepared = prepare(&driver, &child, &id, "continue").unwrap();
    let bytes = driver.read(|tree| {
        let row = tree.command(&id).unwrap();
        serde_json::to_string(&serde_json::json!({
            "command_id": row.command_id, "recipient": row.recipient,
            "attempt_id": row.attempt_id, "kind": row.kind, "text": row.text,
            "message_ids": row.message_ids, "withdrawn": row.withdrawn,
            "fact": row.fact, "turn_id": row.turn_id, "accepted_seq": row.accepted_seq,
        })).unwrap()
    }).unwrap();
    assert_eq!(prepared.prompt, "continue\n\nMessage:\nstanding instruction");
    assert_eq!(bytes, r#"{"accepted_seq":null,"attempt_id":"attempt","command_id":"plak:park-wake:c1:1:attempt:2","fact":null,"kind":"wake","message_ids":["m1"],"recipient":"c1","text":"continue\n\nMessage:\nstanding instruction","turn_id":null,"withdrawn":false}"#);
}

struct SecondSaveStore {
    inner: crate::agent_tree::MemoryTreeStore,
    saves: std::sync::atomic::AtomicUsize,
    stale: bool,
}

impl crate::agent_tree::TreeStore for SecondSaveStore {
    fn exists(&self, id: &str) -> Result<bool, crate::agent_tree::TreeStoreError> {
        self.inner.exists(id)
    }
    fn ids(&self) -> Result<Vec<String>, crate::agent_tree::TreeStoreError> {
        self.inner.ids()
    }
    fn load(
        &self,
        id: &str,
    ) -> Result<Option<crate::agent_tree::PersistedTree>, crate::agent_tree::TreeStoreError> {
        self.inner.load(id)
    }
    fn save(
        &self,
        tree: &ProductAgentTree,
    ) -> Result<crate::agent_tree::Revision, crate::agent_tree::TreeStoreError> {
        if self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            return Err(if self.stale {
                crate::agent_tree::TreeStoreError::stale(tree.stored_revision + 1, tree.revision)
            } else {
                crate::agent_tree::TreeStoreError::unavailable("second save refused")
            });
        }
        self.inner.save(tree)
    }
}

#[test]
fn duplicate_projection_second_save_refusal_restores_command_and_cursor() {
    use plakous::sessions::feed::{FeedFact, FeedRecord};
    for stale in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut tree, child) = root_with_child(dir.path(), "duplicate-second-save");
        tree.record_command(AgentCommand::intent(
            "duplicate",
            &child,
            tree.attempt(&child)
                .map(|attempt| attempt.attempt_id.clone()),
            AgentCommandKind::Start,
            "assignment".to_owned(),
            Vec::new(),
        ))
        .unwrap();
        let store = std::sync::Arc::new(SecondSaveStore {
            inner: crate::agent_tree::MemoryTreeStore::new(),
            saves: std::sync::atomic::AtomicUsize::new(0),
            stale,
        });
        let driver = TreeDriver::new(
            std::sync::Arc::new(std::sync::Mutex::new(tree.clone())),
            store,
            tree.vcs_port(),
            None,
        );
        let header = serde_json::from_value(serde_json::json!({
            "session_id": "00000000-0000-4000-8000-000000000001", "seq": 1,
            "branch_id": "00000000-0000-4000-8000-000000000002", "committed_at_ms": 1000,
            "change": {"created": {"genesis": "00000000-0000-4000-8000-000000000003", "revision": 1}}
        })).unwrap();
        let record = FeedRecord::from_store(
            header,
            vec![FeedFact::CommandAccepted {
                command_id: plakous::agent::CommandId::new("duplicate").unwrap(),
                kind: plakous::sessions::feed::FeedCommandKind::Root {
                    origin: plakous::model::TurnOrigin::UserPrompt,
                },
                turn_id: None,
                sequence: 1,
                operation_id: None,
            }],
        );
        // Intent publication is the first save; the duplicate's projection is the second.
        driver
            .save(&mut driver.tree_handle_for_test().lock().unwrap())
            .unwrap();
        let project = |tree: &mut ProductAgentTree| {
            tree.project_feed(
                &child,
                "00000000-0000-4000-8000-000000000001",
                std::slice::from_ref(&record),
                &crate::agent_tree::TreeFeedReads::default(),
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
        };
        assert!(persist_context(&driver, project).is_err());
        driver
            .read(|restored| {
                assert_eq!(restored.command("duplicate"), tree.command("duplicate"));
                assert_eq!(restored.node(&child), tree.node(&child));
                assert_eq!(
                    restored.feed_cursor("00000000-0000-4000-8000-000000000001"),
                    0
                );
            })
            .unwrap();
        persist_context(&driver, project).unwrap();
        assert_eq!(
            driver
                .read(|tree| tree.command("duplicate").unwrap().fact)
                .unwrap(),
            Some(crate::agent_tree::CommandFact::Accepted)
        );
    }
}

fn project_acceptance(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    recipient: &str,
    command: &str,
) {
    use plakous::sessions::feed::{FeedFact, FeedRecord};
    let header = serde_json::from_value(serde_json::json!({
        "session_id": "00000000-0000-4000-8000-000000000001", "seq": 1,
        "branch_id": "00000000-0000-4000-8000-000000000002", "committed_at_ms": 1000,
        "change": {"created": {"genesis": "00000000-0000-4000-8000-000000000003", "revision": 1}}
    }))
    .unwrap();
    let record = FeedRecord::from_store(
        header,
        vec![FeedFact::CommandAccepted {
            command_id: plakous::agent::CommandId::new(command).unwrap(),
            kind: plakous::sessions::feed::FeedCommandKind::Root {
                origin: plakous::model::TurnOrigin::UserPrompt,
            },
            turn_id: None,
            sequence: 1,
            operation_id: None,
        }],
    );
    runtime
        .mutate_then_emit(false, |tree| {
            tree.project_feed(
                recipient,
                "00000000-0000-4000-8000-000000000001",
                &[record],
                &crate::agent_tree::TreeFeedReads::default(),
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
        })
        .unwrap();
}

#[test]
fn duplicate_child_commands_settle_from_views_without_ack_feed_sequences() {
    use crate::agent_tree::CommandFact;
    for retained_from in [0, 1, 100] {
        assert_eq!(
            duplicate_consumed(Some(CommandFact::Accepted), retained_from),
            Ok(false)
        );
        assert_eq!(
            duplicate_consumed(Some(CommandFact::Returned), retained_from),
            Ok(false)
        );
        assert!(duplicate_consumed(Some(CommandFact::Rejected), retained_from).is_err());
    }
    assert!(duplicate_consumed(None, 0).is_err());
    assert_eq!(duplicate_consumed(None, 1), Ok(true));
    assert_eq!(duplicate_consumed(None, 100), Ok(true));
}

fn complete(tree: &mut ProductAgentTree) {
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    tree.submit_result(
        &child.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.to_owned(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
}

#[tokio::test]
async fn prepared_child_context_is_exact_with_new_mail_and_requires_ack_before_new_command() {
    let dir = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &id,
        "root",
        "owner",
        dir.path().to_owned(),
        "selected",
        WriterMode::SingleLease,
    )
    .unwrap();
    complete(&mut tree);
    let store = std::sync::Arc::new(crate::agent_tree::MemoryTreeStore::new());
    let runtime =
        crate::topology_runtime::insert_root_with(id.clone(), tree, store.clone(), None).unwrap();
    let prepared = prepare(runtime.driver(), "root", "command-one", "continue").unwrap();
    runtime
        .mutate_then_emit(false, |tree| {
            complete(tree);
            Ok(())
        })
        .unwrap();
    crate::topology_runtime::force_release(&id).await;
    let persisted = store.load(&id).unwrap().unwrap();
    let runtime = crate::topology_runtime::insert_root_with(
        id.clone(),
        ProductAgentTree::reopen(persisted),
        store,
        None,
    )
    .unwrap();
    let replay = prepare(runtime.driver(), "root", "command-one", "continue").unwrap();
    assert_eq!(prepared.prompt, replay.prompt);
    assert_eq!(replay.message_ids.len(), 1);
    assert!(prepare(runtime.driver(), "root", "command-one", "changed").is_err());
    assert!(prepare(runtime.driver(), "root", "command-two", "continue").is_err());
    project_acceptance(&runtime, "root", "command-one");
    acknowledge(runtime.driver(), "command-one", Submission::Recorded).unwrap();
    acknowledge(runtime.driver(), "command-one", Submission::Recorded).unwrap();
    let second = prepare(runtime.driver(), "root", "command-two", "continue").unwrap();
    assert_eq!(second.message_ids.len(), 1);
    assert_ne!(second.message_ids, prepared.message_ids);
    acknowledge(runtime.driver(), "command-two", Submission::Refused).unwrap();
    let third = prepare(runtime.driver(), "root", "command-three", "continue").unwrap();
    assert_eq!(
        third.message_ids, second.message_ids,
        "definitively rejected commands do not consume results"
    );
    assert!(
        acknowledge(runtime.driver(), "command-two", Submission::Recorded).is_err(),
        "a terminal rejected context cannot change outcome"
    );
    crate::topology_runtime::force_release(&id).await;
}

fn parent_mail(
    agent_id: &str,
    message: &str,
) -> crate::agent_tree_protocol::SendAgentMessageRequest {
    crate::agent_tree_protocol::SendAgentMessageRequest {
        agent_id: agent_id.to_owned(),
        message: message.to_owned(),
        purpose: None,
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    }
}

fn root_with_child(dir: &std::path::Path, id: &str) -> (ProductAgentTree, String) {
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        id,
        "root",
        "owner",
        dir.to_owned(),
        "selected",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    (tree, child)
}

/// A mail Start also carries the other mail still unread for the child
/// (an interrupted child's mail has no wake to carry it), and its
/// accepted settlement acknowledges all of it.
#[tokio::test]
async fn a_mail_start_carries_other_unread_mail_and_acknowledges_it() {
    let dir = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let (mut tree, child) = root_with_child(dir.path(), &id);
    let earlier = tree
        .send_agent_message("root", &parent_mail(&child, "use the v2 schema"), None)
        .unwrap();
    let started = tree
        .send_agent_message("root", &parent_mail(&child, "resume"), None)
        .unwrap();
    let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None).unwrap();
    let command = format!("plak:mail:{}:start", started.message_id);
    let prepared = prepare(
        runtime.driver(),
        &child,
        &command,
        "assignment\n\nMessage:\nresume",
    )
    .unwrap();
    assert_eq!(
        prepared.prompt,
        "assignment\n\nMessage:\nresume\n\nMessage:\nuse the v2 schema"
    );
    assert_eq!(
        prepared.message_ids,
        [started.message_id.clone(), earlier.message_id.clone()]
    );
    project_acceptance(&runtime, &child, &command);
    acknowledge(runtime.driver(), &command, Submission::Recorded).unwrap();
    assert!(
        runtime
            .read(|tree| tree.wake_mail(&child).is_empty())
            .unwrap()
    );
    crate::topology_runtime::force_release(&id).await;
}

#[tokio::test]
async fn followup_and_steer_primary_mail_acknowledgement_requires_a_feed_fact() {
    for kind in ["follow-up", "steer"] {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let (mut tree, child) = root_with_child(dir.path(), &id);
        let sent = tree
            .send_agent_message("root", &parent_mail(&child, "status?"), None)
            .unwrap();
        let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None).unwrap();
        let command = format!("plak:mail:{}:{kind}", sent.message_id);
        let prepared = prepare(runtime.driver(), &child, &command, "status?").unwrap();
        assert_eq!(prepared.message_ids, std::slice::from_ref(&sent.message_id));
        assert!(acknowledge(runtime.driver(), &command, Submission::Recorded).is_err());
        assert_eq!(
            runtime
                .read(|tree| tree.mail(&sent.message_id).unwrap().durable)
                .unwrap(),
            MailDurableState::Accepted
        );
        project_acceptance(&runtime, &child, &command);
        acknowledge(runtime.driver(), &command, Submission::Recorded).unwrap();
        assert_eq!(
            runtime
                .read(|tree| tree.mail(&sent.message_id).unwrap().durable)
                .unwrap(),
            MailDurableState::Acknowledged
        );
        crate::topology_runtime::force_release(&id).await;
    }
}

/// A batch left prepared by an earlier attempt's wake never blocks the
/// current attempt's commands; one of the current attempt still does.
#[tokio::test]
async fn another_attempts_prepared_wake_does_not_block_a_new_command() {
    let dir = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let (mut tree, child) = root_with_child(dir.path(), &id);
    tree.send_agent_message("root", &parent_mail(&child, "status?"), None)
        .unwrap();
    let attempt = tree.attempt(&child).unwrap().attempt_id.as_str().to_owned();
    let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None).unwrap();
    let stale = format!(
        "{}{child}:1:{child}-a9:3",
        super::super::park::WAKE_COMMAND_PREFIX
    );
    assert_eq!(
        command_attempt(&stale),
        Some(format!("{child}-a9").as_str())
    );
    let held = prepare(runtime.driver(), &child, &stale, "wake").unwrap();
    // The attempt that journaled it ended; the current one carries on.
    assert_eq!(held.message_ids.len(), 1, "the stale wake holds the mail");
    prepare(
        runtime.driver(),
        &child,
        "plak:spawn-start:t:c",
        "assignment",
    )
    .unwrap();
    let current = format!(
        "{}{child}:1:{attempt}:3",
        super::super::park::WAKE_COMMAND_PREFIX
    );
    assert_eq!(command_attempt(&current), Some(attempt.as_str()));
    let blocking = prepare(runtime.driver(), &child, &current, "wake").unwrap();
    assert_eq!(blocking.message_ids.len(), 1);
    assert!(
        prepare(runtime.driver(), &child, "plak:other:command", "assignment").is_err(),
        "a prepared batch of the current attempt still blocks"
    );
    crate::topology_runtime::force_release(&id).await;
}

#[tokio::test]
async fn command_prepared_without_results_does_not_gain_new_results_on_retry() {
    let dir = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let tree = ProductAgentTree::bootstrap_root_for_test(
        &id,
        "root",
        "owner",
        dir.path().to_owned(),
        "selected",
        WriterMode::SingleLease,
    )
    .unwrap();
    let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None).unwrap();
    let prepared = prepare(runtime.driver(), "root", "command-one", "continue").unwrap();
    runtime
        .mutate_then_emit(false, |tree| {
            complete(tree);
            Ok(())
        })
        .unwrap();
    let replay = prepare(runtime.driver(), "root", "command-one", "continue").unwrap();
    assert_eq!(prepared.prompt, replay.prompt);
    assert_eq!(replay.message_ids, [] as [String; 0]);
    assert_eq!(
        runtime
            .read(|tree| tree.pending_result_mail("root").len())
            .unwrap(),
        1
    );
    crate::topology_runtime::force_release(&id).await;
}
