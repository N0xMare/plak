#![allow(clippy::unwrap_used, reason = "test assertions")]

use super::{
    AgentCommand, AgentCommandKind, TreeDriver, acknowledge, command_attempt, duplicate_consumed,
    persist_context,
};
use crate::agent::ledger::ChildLedger;
use crate::agent_tree::{MailDurableState, ProductAgentTree, TreeStore as _};
use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, WriterMode};
use crate::driver::ledger::Settlement;
use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger};

#[test]
fn explicit_child_intents_match_pre_port_rows_and_prompts() {
    use super::ChildLedger;
    use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger};

    // These are the exact pre-port spellings and batch rules, including
    // follow-up and steer's shared stored kind. Each runs against a fresh
    // mailbox so selection order cannot hide a dropped primary envelope.
    for (id, kind, stored, all_mail, mail_start, delivered) in [
        (
            "plak:spawn-start:ledger-bytes:c1",
            CommandKind::Start,
            AgentCommandKind::Start,
            false,
            false,
            false,
        ),
        (
            "plak:park-wake:c1:1:attempt:2",
            CommandKind::Wake,
            AgentCommandKind::Wake,
            true,
            false,
            false,
        ),
        (
            "plak:resume-transient:c1:attempt:1",
            CommandKind::Resume,
            AgentCommandKind::Resume,
            true,
            false,
            false,
        ),
        (
            "plak:submit-harness:c1:1:attempt:2",
            CommandKind::Corrective,
            AgentCommandKind::Corrective,
            false,
            false,
            false,
        ),
        (
            "plak:mail:m1:start",
            CommandKind::MailStart,
            AgentCommandKind::MailStart,
            false,
            true,
            true,
        ),
        (
            "plak:mail:m1:follow-up",
            CommandKind::Followup,
            AgentCommandKind::Followup,
            false,
            false,
            true,
        ),
        (
            "plak:mail:m1:steer",
            CommandKind::Followup,
            AgentCommandKind::Followup,
            false,
            false,
            true,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut tree, child) = root_with_child(dir.path(), "ledger-bytes");
        tree.send_agent_message("root", &parent_mail(&child, "standing instruction"), None)
            .unwrap();
        let attempt = if matches!(
            kind,
            CommandKind::Wake | CommandKind::Resume | CommandKind::Corrective
        ) {
            Some(crate::agent_tree_protocol::AttemptId::from_persisted(
                "attempt".to_owned(),
            ))
        } else {
            tree.attempt(&child)
                .map(|attempt| attempt.attempt_id.clone())
        };
        let driver = TreeDriver::new(
            std::sync::Arc::new(std::sync::Mutex::new(tree.clone())),
            std::sync::Arc::new(crate::agent_tree::MemoryTreeStore::new()),
            tree.vcs_port(),
            None,
        );
        let batch = if all_mail {
            BatchRule::WakeAll
        } else if mail_start {
            BatchRule::MailStart("m1")
        } else if delivered {
            BatchRule::Delivered("m1")
        } else {
            BatchRule::Results
        };
        let prepared = ChildLedger::new(&driver, &child)
            .prepare(CommandIntent {
                command_id: id,
                kind,
                base_text: "continue",
                batch,
            })
            .unwrap();
        let expected_text = if all_mail {
            "continue\n\nMessage:\nstanding instruction"
        } else {
            "continue"
        };
        let ids = if all_mail || delivered {
            vec!["m1".to_owned()]
        } else {
            Vec::new()
        };
        assert_eq!(prepared.prompt.as_bytes(), expected_text.as_bytes());
        let expected = AgentCommand::intent(id, &child, attempt, stored, expected_text, ids);
        assert_eq!(
            driver
                .read(|tree| tree.command(id).unwrap().clone())
                .unwrap(),
            expected
        );
    }
}

#[test]
fn mail_start_batched_command_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        "mail-start-bytes",
        "root",
        "owner",
        dir.path().to_owned(),
        "selected",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut snapshot = tree.snapshot.clone();
    snapshot.max_depth = 2;
    snapshot.digest = snapshot.compute_digest();
    tree.install_snapshot(snapshot);
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap()
        .agent_id;
    tree.send_agent_message("root", &parent_mail(&child, "resume"), None)
        .unwrap();
    tree.send_agent_message("root", &parent_mail(&child, "standing instruction"), None)
        .unwrap();
    let grandchild = tree
        .spawn_agent(&child, &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.submit_result(
        &grandchild,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&grandchild).unwrap();
    tree.settle_for_test();
    let driver = TreeDriver::new(
        std::sync::Arc::new(std::sync::Mutex::new(tree.clone())),
        std::sync::Arc::new(crate::agent_tree::MemoryTreeStore::new()),
        tree.vcs_port(),
        None,
    );
    let id = "plak:mail:m1:start";
    let prepared = ChildLedger::new(&driver, &child)
        .prepare(CommandIntent {
            command_id: id,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nresume",
            batch: BatchRule::MailStart("m1"),
        })
        .unwrap();
    let bytes = driver
        .read(|tree| {
            let row = tree.command(id).unwrap();
            serde_json::to_string(&serde_json::json!({
                "command_id": row.command_id, "recipient": row.recipient,
                "attempt_id": row.attempt_id, "kind": row.kind, "text": row.text,
                "message_ids": row.message_ids, "withdrawn": row.withdrawn,
                "fact": row.fact, "turn_id": row.turn_id, "accepted_seq": row.accepted_seq,
            }))
            .unwrap()
        })
        .unwrap();
    assert_eq!(
        bytes.as_bytes(),
        include_str!("fixtures/mail-start-batch.json")
            .trim_end()
            .as_bytes()
    );
    assert_eq!(prepared.message_ids, ["m1", "m2", "m3"]);
}

#[test]
fn child_ledger_prechange_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut tree, child) = root_with_child(dir.path(), "ledger-bytes");
    tree.send_agent_message("root", &parent_mail(&child, "standing instruction"), None)
        .unwrap();
    let driver = TreeDriver::new(
        std::sync::Arc::new(std::sync::Mutex::new(tree.clone())),
        std::sync::Arc::new(crate::agent_tree::MemoryTreeStore::new()),
        tree.vcs_port(),
        None,
    );
    let id = format!("plak:park-wake:{child}:1:attempt:2");
    let prepared = ChildLedger::new(&driver, &child)
        .prepare(CommandIntent {
            command_id: &id,
            kind: CommandKind::Wake,
            base_text: "continue",
            batch: BatchRule::WakeAll,
        })
        .unwrap();
    let bytes = driver
        .read(|tree| {
            let row = tree.command(&id).unwrap();
            serde_json::to_string(&serde_json::json!({
                "command_id": row.command_id, "recipient": row.recipient,
                "attempt_id": row.attempt_id, "kind": row.kind, "text": row.text,
                "message_ids": row.message_ids, "withdrawn": row.withdrawn,
                "fact": row.fact, "turn_id": row.turn_id, "accepted_seq": row.accepted_seq,
            }))
            .unwrap()
        })
        .unwrap();
    assert_eq!(
        prepared.prompt,
        "continue\n\nMessage:\nstanding instruction"
    );
    assert_eq!(
        bytes,
        r#"{"accepted_seq":null,"attempt_id":"attempt","command_id":"plak:park-wake:c1:1:attempt:2","fact":null,"kind":"wake","message_ids":["m1"],"recipient":"c1","text":"continue\n\nMessage:\nstanding instruction","turn_id":null,"withdrawn":false}"#
    );
}

#[test]
fn child_integrity_refusals_preserve_the_intent() {
    use crate::driver::ledger::LedgerError;
    let dir = tempfile::tempdir().unwrap();
    let (tree, child) = root_with_child(dir.path(), "integrity");
    let driver = TreeDriver::new(
        std::sync::Arc::new(std::sync::Mutex::new(tree.clone())),
        std::sync::Arc::new(crate::agent_tree::MemoryTreeStore::new()),
        tree.vcs_port(),
        None,
    );
    ChildLedger::new(&driver, &child)
        .prepare(CommandIntent {
            command_id: "intent",
            kind: CommandKind::Start,
            base_text: "work",
            batch: BatchRule::Results,
        })
        .unwrap();
    let before = driver
        .read(|tree| (tree.revision, tree.command("intent").cloned()))
        .unwrap();
    let conflict = ChildLedger::new(&driver, &child).prepare(CommandIntent {
        command_id: "intent",
        kind: CommandKind::Start,
        base_text: "changed",
        batch: BatchRule::Results,
    });
    assert!(matches!(conflict, Err(LedgerError::Integrity(_))));
    for settlement in [
        Settlement::Recorded,
        Settlement::Refused {
            pause: Some(
                crate::sessions::projector::HostPauseCause::UnrecordedRefusal {
                    reason: "refused".into(),
                },
            ),
        },
    ] {
        assert!(matches!(
            ChildLedger::settlement_only(&driver).settle("intent", settlement),
            Err(LedgerError::Integrity(_))
        ));
    }
    assert_eq!(
        before,
        driver
            .read(|tree| (tree.revision, tree.command("intent").cloned()))
            .unwrap()
    );
}

#[test]
fn interrupted_tree_capacity_refusal_keeps_exact_child_intents_retryable() {
    use crate::driver::ledger::LedgerError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let (mut tree, child) = root_with_child(dir.path(), "ledger-capacity");
    tree.send_agent_message("root", &parent_mail(&child, "continue"), None)
        .unwrap();
    let store = std::sync::Arc::new(SecondSaveStore {
        inner: crate::agent_tree::MemoryTreeStore::new(),
        saves: AtomicUsize::new(0),
        stale: false,
        full: true,
    });
    let driver = TreeDriver::new(
        std::sync::Arc::new(std::sync::Mutex::new(tree.clone())),
        store.clone(),
        tree.vcs_port(),
        None,
    );
    let command = format!("plak:park-wake:{child}:1:c1-a1:1");
    let mut ledger = ChildLedger::new(&driver, &child);
    let prepare = |ledger: &mut ChildLedger<'_>| {
        ledger.prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::Wake,
            base_text: "wake",
            batch: BatchRule::WakeAll,
        })
    };
    assert!(
        matches!(prepare(&mut ledger), Err(LedgerError::TreeStorageFull(error))
        if error.code == crate::agent_tree_protocol::TopologyErrorCode::RecoveryStorageUnavailable)
    );
    driver
        .read(|tree| {
            assert!(tree.command(&command).is_none());
            assert_eq!(tree.mail("m1").unwrap().durable, MailDurableState::Accepted);
        })
        .unwrap();
    let prepared = prepare(&mut ledger).unwrap();
    assert_eq!(prepared.message_ids, ["m1"]);
    let before = driver.read(|tree| tree.command(&command).cloned()).unwrap();
    store.saves.store(1, Ordering::SeqCst);
    let refused = ledger.settle(&command, Settlement::Refused { pause: None });
    let Err(LedgerError::TreeStorageFull(error)) = refused else {
        panic!("tree capacity refusal must remain typed")
    };
    let product = crate::session_store::RecordStoreError::from(LedgerError::TreeStorageFull(error));
    assert_eq!(
        product.refusal(),
        Some(crate::error::StoreRefusal::StorageFull)
    );
    assert_eq!(
        before,
        driver.read(|tree| tree.command(&command).cloned()).unwrap()
    );
    ledger
        .settle(&command, Settlement::Refused { pause: None })
        .unwrap();
    driver
        .read(|tree| {
            assert!(tree.command(&command).unwrap().withdrawn);
            assert_eq!(tree.mail("m1").unwrap().durable, MailDurableState::Accepted);
        })
        .unwrap();
}

struct SecondSaveStore {
    inner: crate::agent_tree::MemoryTreeStore,
    saves: std::sync::atomic::AtomicUsize,
    stale: bool,
    full: bool,
}

impl crate::agent_tree::TreeStore for SecondSaveStore {
    fn storage_wait_cancelled(&self) -> bool {
        self.full
    }

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
            return Err(if self.full {
                crate::agent_tree::TreeStoreError::storage_full()
            } else if self.stale {
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
            full: false,
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
        assert!(matches!(
            persist_context(&driver, project),
            Err(crate::driver::ledger::LedgerError::Unavailable(_))
        ));
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
    let prepared = ChildLedger::new(runtime.driver(), "root")
        .prepare(CommandIntent {
            command_id: "command-one",
            kind: CommandKind::Followup,
            base_text: "continue",
            batch: BatchRule::Results,
        })
        .unwrap();
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
    let replay = ChildLedger::new(runtime.driver(), "root")
        .prepare(CommandIntent {
            command_id: "command-one",
            kind: CommandKind::Followup,
            base_text: "continue",
            batch: BatchRule::Results,
        })
        .unwrap();
    assert_eq!(prepared.prompt, replay.prompt);
    assert_eq!(replay.message_ids.len(), 1);
    assert!(
        ChildLedger::new(runtime.driver(), "root")
            .prepare(CommandIntent {
                command_id: "command-one",
                kind: CommandKind::Followup,
                base_text: "changed",
                batch: BatchRule::Results
            })
            .is_err()
    );
    assert!(
        ChildLedger::new(runtime.driver(), "root")
            .prepare(CommandIntent {
                command_id: "command-two",
                kind: CommandKind::Followup,
                base_text: "continue",
                batch: BatchRule::Results
            })
            .is_err()
    );
    project_acceptance(&runtime, "root", "command-one");
    acknowledge(runtime.driver(), "command-one", Settlement::Recorded).unwrap();
    acknowledge(runtime.driver(), "command-one", Settlement::Recorded).unwrap();
    let second = ChildLedger::new(runtime.driver(), "root")
        .prepare(CommandIntent {
            command_id: "command-two",
            kind: CommandKind::Followup,
            base_text: "continue",
            batch: BatchRule::Results,
        })
        .unwrap();
    assert_eq!(second.message_ids.len(), 1);
    assert_ne!(second.message_ids, prepared.message_ids);
    acknowledge(
        runtime.driver(),
        "command-two",
        Settlement::Refused { pause: None },
    )
    .unwrap();
    let third = ChildLedger::new(runtime.driver(), "root")
        .prepare(CommandIntent {
            command_id: "command-three",
            kind: CommandKind::Followup,
            base_text: "continue",
            batch: BatchRule::Results,
        })
        .unwrap();
    assert_eq!(
        third.message_ids, second.message_ids,
        "definitively rejected commands do not consume results"
    );
    assert!(
        acknowledge(runtime.driver(), "command-two", Settlement::Recorded).is_err(),
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
    let prepared = ChildLedger::new(runtime.driver(), &child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nresume",
            batch: BatchRule::MailStart(&started.message_id),
        })
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
    acknowledge(runtime.driver(), &command, Settlement::Recorded).unwrap();
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
        let prepared = ChildLedger::new(runtime.driver(), &child)
            .prepare(CommandIntent {
                command_id: &command,
                kind: CommandKind::Followup,
                base_text: "status?",
                batch: BatchRule::Delivered(&sent.message_id),
            })
            .unwrap();
        assert_eq!(prepared.message_ids, std::slice::from_ref(&sent.message_id));
        assert!(acknowledge(runtime.driver(), &command, Settlement::Recorded).is_err());
        assert_eq!(
            runtime
                .read(|tree| tree.mail(&sent.message_id).unwrap().durable)
                .unwrap(),
            MailDurableState::Accepted
        );
        project_acceptance(&runtime, &child, &command);
        acknowledge(runtime.driver(), &command, Settlement::Recorded).unwrap();
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
    let stale = format!("{}{child}:1:{child}-a9:3", "plak:park-wake:");
    assert_eq!(
        command_attempt(&stale),
        Some(format!("{child}-a9").as_str())
    );
    let held = ChildLedger::new(runtime.driver(), &child)
        .prepare(CommandIntent {
            command_id: &stale,
            kind: CommandKind::Wake,
            base_text: "wake",
            batch: BatchRule::WakeAll,
        })
        .unwrap();
    // The attempt that journaled it ended; the current one carries on.
    assert_eq!(held.message_ids.len(), 1, "the stale wake holds the mail");
    ChildLedger::new(runtime.driver(), &child)
        .prepare(CommandIntent {
            command_id: "plak:spawn-start:t:c",
            kind: CommandKind::Start,
            base_text: "assignment",
            batch: BatchRule::Results,
        })
        .unwrap();
    let current = format!("{}{child}:1:{attempt}:3", "plak:park-wake:");
    assert_eq!(command_attempt(&current), Some(attempt.as_str()));
    let blocking = ChildLedger::new(runtime.driver(), &child)
        .prepare(CommandIntent {
            command_id: &current,
            kind: CommandKind::Wake,
            base_text: "wake",
            batch: BatchRule::WakeAll,
        })
        .unwrap();
    assert_eq!(blocking.message_ids.len(), 1);
    assert!(
        ChildLedger::new(runtime.driver(), &child)
            .prepare(CommandIntent {
                command_id: "plak:other:command",
                kind: CommandKind::Followup,
                base_text: "assignment",
                batch: BatchRule::Results
            })
            .is_err(),
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
    let prepared = ChildLedger::new(runtime.driver(), "root")
        .prepare(CommandIntent {
            command_id: "command-one",
            kind: CommandKind::Followup,
            base_text: "continue",
            batch: BatchRule::Results,
        })
        .unwrap();
    runtime
        .mutate_then_emit(false, |tree| {
            complete(tree);
            Ok(())
        })
        .unwrap();
    let replay = ChildLedger::new(runtime.driver(), "root")
        .prepare(CommandIntent {
            command_id: "command-one",
            kind: CommandKind::Followup,
            base_text: "continue",
            batch: BatchRule::Results,
        })
        .unwrap();
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
