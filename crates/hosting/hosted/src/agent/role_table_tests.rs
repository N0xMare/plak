//! The child column of the intent x role table, for the cells that need a
//! live child turn or the tree's grants. See `crate::role_table`.

#![allow(clippy::unwrap_used, reason = "role table tests")]

use std::sync::{Arc, Mutex};

use serde_json::json;

use super::park::tests::{
    Fixture, fixture, fixture_deciding, held, leaf, parked, settled, start_job, turns,
};
use crate::agent_tree::{MailDisposition, NodeDetach, ProductAgentTree, TopologyObserver};
use crate::agent_tree_protocol::{
    AttemptScopedRequest, AttemptStatus, MessagePriority, MessagePurpose, NodeStatus,
    SendAgentMessageRequest, SpawnAgentRequest, TopologyErrorCode, WriterMode,
};
use crate::agent_tree_tools::{MailDeliveryAck, MailDeliveryFailure, MailDeliveryRequest};
use crate::role_table::{Cell, Class, Column, Covered, Row, UserAction, assert_today};

pub(crate) const COVERED: &[Covered] = &[
    Covered {
        row: Row::Prompt,
        column: Column::Child,
        test: child_prompt_is_queued_mail_delivered_into_the_running_turn,
    },
    Covered {
        row: Row::Steer,
        column: Column::Child,
        test: child_steer_is_urgent_mail_steered_into_the_running_turn,
    },
    Covered {
        row: Row::Interrupt,
        column: Column::Child,
        test: child_interrupt_ends_the_turn_and_keeps_the_attempt,
    },
    Covered {
        row: Row::Processes,
        column: Column::Child,
        test: child_process_stop_reaches_its_job_and_background_has_no_route,
    },
    Covered {
        row: Row::Approval,
        column: Column::Child,
        test: child_approval_is_decided_once_by_the_child,
    },
    Covered {
        row: Row::Quit,
        column: Column::Child,
        test: child_quit_detaches_without_ending_the_attempt,
    },
    Covered {
        row: Row::Navigator,
        column: Column::Child,
        test: child_tree_operations_are_refused_to_a_child_caller,
    },
    Covered {
        row: Row::InvokeTool,
        column: Column::Child,
        test: child_invoke_tool_runs_as_the_named_child,
    },
    Covered {
        row: Row::ChildOnly,
        column: Column::Child,
        test: child_only_events_start_submit_and_report_the_result,
    },
];

fn mail(fixture: &Fixture, priority: Option<MessagePriority>) -> SendAgentMessageRequest {
    SendAgentMessageRequest {
        refresh: None,
        agent_id: fixture.child.clone(),
        message: "also check the docs".into(),
        purpose: None,
        priority,
        in_reply_to: None,
        output_schema_json: None,
    }
}

/// Delivers one accepted message the way the send does after the tree
/// chose its disposition.
async fn deliver(
    fixture: &Fixture,
    message_id: &str,
    disposition: MailDisposition,
) -> Result<MailDeliveryAck, MailDeliveryFailure> {
    let (locator, path) = fixture
        .runtime
        .read(|tree| {
            let node = tree.node(&fixture.child).unwrap();
            (
                node.session_locator.clone().unwrap(),
                node.path.display().to_string(),
            )
        })
        .unwrap();
    let deliverer = super::children::ProductMailDeliverer {
        parent: crate::agent::SpawnParent::Api(super::tests::openrouter_test_parent()),
        runtime: Arc::clone(&fixture.runtime),
    };
    deliverer
        .deliver(MailDeliveryRequest {
            agent_id: fixture.child.clone(),
            session_locator: locator,
            disposition,
            message_id: message_id.into(),
            body: "also check the docs".into(),
            description: String::new(),
            prompt: String::new(),
            output_schema_json: String::new(),
            path,
            turn_token: 1,
        })
        .await
}

async fn live_turn(fixture: &Fixture) -> String {
    let live = fixture.runtime.live.lock().await;
    live.get(&fixture.child).unwrap().turn_id().to_string()
}

async fn finish(fixture: &Fixture) {
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
}

/// B: deferred mail to a busy child queues, and its delivery is a
/// follow-up into the turn already running.
#[tokio::test]
async fn child_prompt_is_queued_mail_delivered_into_the_running_turn() {
    assert_today(Row::Prompt, Column::Child, Cell::Classes(&[Class::B]));
    let fixture = fixture(
        json!([held("working"), {"output": {"text": "read the follow-up"}}]),
        leaf,
    )
    .await;
    let sent = fixture
        .mutate(|tree| {
            tree.send_agent_message(
                "root",
                &mail(&fixture, Some(MessagePriority::Deferred)),
                None,
            )
            .unwrap()
        })
        .await;
    assert_eq!(sent.disposition, MailDisposition::Queued);
    let running = live_turn(&fixture).await;
    let ack = deliver(&fixture, &sent.message_id, sent.disposition)
        .await
        .unwrap();
    assert_eq!(ack.turn_id.as_deref(), Some(running.as_str()));
    finish(&fixture).await;
}

/// B: urgent mail to a busy child steers the running turn.
#[tokio::test]
async fn child_steer_is_urgent_mail_steered_into_the_running_turn() {
    assert_today(Row::Steer, Column::Child, Cell::Classes(&[Class::B]));
    let fixture = fixture(
        json!([held("working"), {"output": {"text": "steered"}}]),
        leaf,
    )
    .await;
    let sent = fixture
        .mutate(|tree| {
            tree.send_agent_message("root", &mail(&fixture, Some(MessagePriority::Urgent)), None)
                .unwrap()
        })
        .await;
    assert_eq!(sent.disposition, MailDisposition::Steered);
    let running = live_turn(&fixture).await;
    let ack = deliver(&fixture, &sent.message_id, sent.disposition)
        .await
        .unwrap();
    assert_eq!(ack.turn_id.as_deref(), Some(running.as_str()));
    finish(&fixture).await;
}

/// U: `interrupt_agent` is taken at once on a running child, ends only its
/// turn, and leaves the attempt open to more mail.
#[tokio::test]
async fn child_interrupt_ends_the_turn_and_keeps_the_attempt() {
    assert_today(Row::Interrupt, Column::Child, Cell::Classes(&[Class::U]));
    let fixture = fixture(json!([held("working")]), leaf).await;
    let status = fixture
        .mutate(|tree| {
            tree.interrupt_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap()
        })
        .await;
    assert_eq!(status, NodeStatus::Interrupted);
    fixture.runtime.cancel_live(&fixture.child).await.unwrap();
    fixture
        .until("the turn to end", |tree, child| !tree.has_turn_owner(child))
        .await;
    let attempt = fixture
        .runtime
        .read(|tree| tree.attempt(&fixture.child).unwrap().status)
        .unwrap();
    assert_eq!(attempt, AttemptStatus::Running, "resumable, not ended");
    fixture
        .mutate(|tree| tree.send_agent_message("root", &mail(&fixture, None), None))
        .await
        .expect("an interrupted child still takes mail");
}

/// B: the user stop the root's `StopProcesses` runs reaches a child's own
/// job, and its end wakes the parked child.
#[tokio::test]
async fn child_process_stop_reaches_its_job_and_background_has_no_route() {
    assert_today(
        Row::Processes,
        Column::Child,
        Cell::Classes(&[Class::B, Class::R]),
    );
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let rows = fixture.registry.snapshot();
    assert_eq!(rows.len(), 1, "{rows:?}");
    fixture.registry.stop_user(Some(&rows[0].id)).unwrap();
    fixture.until("the wake turn", turns(2)).await;
    // B1: expects B (`Background` moves the child's own foreground call).
    // Today `Background` names no node and only the root turn takes it.
    assert_eq!(
        crate::role_table::child_address(&UserAction::Background),
        None
    );
    finish(&fixture).await;
}

/// B: a decision a child's turn waits on routes by its handle to that
/// child, whose actor is its single decider: the first answer resumes the
/// turn, and a later one, like any handle no live child holds, is not
/// routed and gets the root's refusal.
#[tokio::test]
async fn child_approval_is_decided_once_by_the_child() {
    assert_today(Row::Approval, Column::Child, Cell::Classes(&[Class::B]));
    let fixture = fixture_deciding(json!([start_job("true"), held("decided")]), leaf).await;
    let approval_id = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let pending = {
                let live = fixture.runtime.live.lock().await;
                live.get(&fixture.child)
                    .and_then(crate::runtime::child_projection::of)
                    .map(|observation| observation.pending_decisions())
                    .unwrap_or_default()
            };
            if let Some(approval_id) = pending.into_iter().next() {
                break approval_id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the child's turn waits on its decision");
    let decide = |approval_id: String, allow: bool| {
        let live = Arc::clone(&fixture.runtime.live);
        async move { crate::topology_runtime::decide_live(&live, &approval_id, allow).await }
    };
    assert_eq!(decide(approval_id.clone(), true).await, Some(Ok(())));
    // The loser's handle is consumed, so it reads like one never offered
    // and the root refuses it with the one indistinguishable note.
    assert_eq!(decide(approval_id, false).await, None, "the second answer");
    assert_eq!(decide("approval-unknown".to_owned(), true).await, None);
    // The allowed effect ran and the turn went on to its next round.
    fixture.held_reached().await;
    // The policy that pends is the fixture's; the product's stays
    // allow-registered, so product children never wait on a decision.
    assert_eq!(
        crate::agent::session_tool_policy(crate::agent::SessionToolFence::Full)
            .revision()
            .policy_id(),
        "plakous.allow-registered"
    );
    finish(&fixture).await;
}

#[derive(Default)]
struct Detaches(Mutex<Vec<(String, NodeDetach)>>);

impl TopologyObserver for Detaches {
    fn turn_usage(&self, _terminal: crate::agent_tree::ProjectedChildTurnUsage) {}
    fn node_spawned(&self, _: &str, _: &str, _: Option<&str>, _: &str) {}
    fn node_detached(&self, _: &str, node_id: &str, _: Option<&str>, status: NodeDetach) {
        self.0.lock().unwrap().push((node_id.to_owned(), status));
    }
    fn epoch_advanced(&self, _: &str) {}
}

/// Detach: the host's stop detaches the child's attachment once and leaves
/// its node and attempt as they were; stopping the child itself is a close
/// or an interrupt.
#[tokio::test]
async fn child_quit_detaches_without_ending_the_attempt() {
    assert_today(Row::Quit, Column::Child, Cell::Detach);
    let fixture = fixture(json!([held("working")]), leaf).await;
    let detaches = Arc::new(Detaches::default());
    let observer: Arc<dyn TopologyObserver> = detaches.clone();
    let (status, attempt) = fixture
        .mutate(|tree| {
            tree.emit_shutdown_detaches(Some(&observer));
            tree.emit_shutdown_detaches(Some(&observer));
            (
                tree.node(&fixture.child).unwrap().status,
                tree.attempt(&fixture.child).unwrap().status,
            )
        })
        .await;
    assert_eq!(
        *detaches.0.lock().unwrap(),
        [(fixture.child.clone(), NodeDetach::Shutdown)]
    );
    assert!(!status.is_finished(), "{status:?}");
    assert_eq!(attempt, AttemptStatus::Running);
    finish(&fixture).await;
}

/// R: a child caller has no tree-wide operation: stop-all, and an
/// interrupt or close of a node it does not manage, are refused.
#[test]
fn child_tree_operations_are_refused_to_a_child_caller() {
    assert_today(Row::Navigator, Column::Child, Cell::Classes(&[Class::R]));
    let root = tempfile::tempdir().unwrap();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        "role-table-navigator",
        "root",
        "owner",
        root.path().to_owned(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let first = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    let second = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    let sibling = AttemptScopedRequest {
        agent_id: second.clone(),
        expected_attempt_id: None,
    };
    let refusals = [
        tree.stop_all(&first, None).map(|_| ()),
        tree.interrupt_agent(&first, &sibling, None).map(|_| ()),
        tree.close_agent(&first, &sibling, None).map(|_| ()),
    ];
    for refusal in refusals {
        assert_eq!(refusal.unwrap_err().code, TopologyErrorCode::GrantDenied);
    }
    assert_ne!(tree.node(&second).unwrap().status, NodeStatus::Closed);
    tree.stop_all("root", None)
        .expect("the root face runs the same operation");
}

/// I: `InvokeTool` names a child as the caller, and the hub runs the tool
/// under that child's grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_invoke_tool_runs_as_the_named_child() {
    assert_today(Row::InvokeTool, Column::Child, Cell::Classes(&[Class::I]));
    let root = tempfile::tempdir().unwrap();
    let tree_id = format!("role-table-invoke-{}", uuid::Uuid::new_v4());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id.clone(),
        "root",
        "owner",
        root.path().to_owned(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None)
        .await
        .unwrap();
    let receipt = crate::agent::invoke_topology_tool_on_hub(
        &tree_id,
        Some(&child),
        "list_agents",
        &json!({}),
        &uuid::Uuid::new_v4().to_string(),
    )
    .await;
    crate::topology_runtime::force_release(&tree_id).await;
    let receipt = receipt.unwrap();
    assert!(receipt.succeeded, "{}", receipt.detail);
}

/// The child role policy: the spawn's Start runs the assignment, the
/// child's own `submit_result` ends the attempt, and the result effect
/// mails the parent once.
#[tokio::test]
async fn child_only_events_start_submit_and_report_the_result() {
    assert_today(Row::ChildOnly, Column::Child, Cell::ChildPolicy);
    assert_eq!(
        crate::role_table::intents_of(Row::ChildOnly),
        [] as [UserAction; 0]
    );
    let fixture = fixture(json!([held("working")]), leaf).await;
    let started = fixture
        .runtime
        .read(|tree| {
            tree.commands()
                .values()
                .filter(|command| {
                    command.recipient == fixture.child
                        && command.kind == crate::agent_tree::AgentCommandKind::Start
                })
                .count()
        })
        .unwrap();
    assert_eq!(started, 1, "the spawn admitted one Start");
    finish(&fixture).await;
    fixture
        .until("the result mail", |tree, child| {
            tree.pending_mail("root")
                .iter()
                .any(|mail| mail.from == child && mail.purpose == MessagePurpose::Result)
        })
        .await;
    let (status, results) = fixture
        .runtime
        .read(|tree| {
            (
                tree.attempt(&fixture.child).unwrap().status,
                tree.pending_mail("root")
                    .iter()
                    .filter(|mail| mail.purpose == MessagePurpose::Result)
                    .count(),
            )
        })
        .unwrap();
    assert_eq!(status, AttemptStatus::Completed);
    assert_eq!(results, 1, "one result per attempt");
    // A finished attempt takes no ordinary mail: only a followup reopens it.
    let refused = fixture
        .mutate(|tree| tree.send_agent_message("root", &mail(&fixture, None), None))
        .await
        .unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::RecipientAttemptTerminal);
}
