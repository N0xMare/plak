//! Spec 03 section 4.4: a root terminal settles the root's await receipts by
//! what its checkpoint did with the turn's exchange.

#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::{HostLoop, TerminalErrorPresentation, TurnHostOutcome};
use crate::agent::TerminalExchange;
use crate::agent_tree::{MailDurableState, ProductAgentTree};
use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, WriterMode};
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::fresh_injected_attached_host;
use crate::topology_runtime::ProductTopologyRuntime;
use std::sync::Arc;
use std::time::Instant;

/// One host over a tree whose root holds one unread Result envelope.
pub(super) fn with_unread_result(
    test: impl FnOnce(&mut HostLoop<'_, TestExtension>, &Arc<ProductTopologyRuntime>),
) {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(dir.path());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &config.session.id,
        "root",
        "owner",
        config.session.workspace_root.clone(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    tree.submit_result(
        &child.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    let runtime =
        crate::topology_runtime::insert_root(config.session.id.clone(), tree, None).unwrap();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, _observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(
        config,
        &receiver,
        crate::runtime::Sender::over(events),
        None,
    )
    .unwrap();
    test(&mut host, &runtime);
    host.release().unwrap();
}

/// A bare wait under a fresh call id, which canonical history never
/// holds: it acknowledges the unread envelope and keeps one receipt.
pub(super) fn wait_outside_history(
    host: &HostLoop<'_, TestExtension>,
    runtime: &Arc<ProductTopologyRuntime>,
) {
    let tree_id = runtime.read(|tree| tree.tree_id.clone()).unwrap();
    let receipt = host
        .executor
        .block_on(crate::agent::invoke_topology_tool_on_hub(
            &tree_id,
            None,
            "wait_agent",
            &serde_json::json!({ "timeout_ms": 1 }),
            &uuid::Uuid::new_v4().to_string(),
        ))
        .unwrap()
        .unwrap();
    assert!(receipt.succeeded, "{}", receipt.detail);
    assert_eq!(receipts_and_unread(runtime), (1, 0));
}

pub(super) fn receipts_and_unread(runtime: &Arc<ProductTopologyRuntime>) -> (usize, usize) {
    runtime
        .read(|tree| {
            (
                tree.retained_wait_receipts("root").len(),
                tree.pending_mail("root").len(),
            )
        })
        .unwrap()
}

/// A failure the driver did not close (`Internal`) committed none of its
/// final calls: publication settles from committed history and the mail
/// is unread again. A terminal the host never observed leaves the
/// receipt alone, and the next prompt settles it before it selects mail,
/// so the restored envelope rides that prompt.
#[test]
fn a_failure_without_a_committed_exchange_returns_wait_mail_to_unread() {
    with_unread_result(|host, runtime| {
        wait_outside_history(host, runtime);
        host.publish_terminal(
            Ok(TurnHostOutcome::Failed {
                transient: None,
                error: "contained panic".into(),
                exchange: TerminalExchange::Unclosed,
                remedy: None,
            }),
            Instant::now(),
            TerminalErrorPresentation::Events,
        );
        assert_eq!(receipts_and_unread(runtime), (0, 1));

        wait_outside_history(host, runtime);
        host.publish_terminal(
            Ok(TurnHostOutcome::Interrupted {
                assistant: String::new(),
                exchange: TerminalExchange::Unresolved,
            }),
            Instant::now(),
            TerminalErrorPresentation::Events,
        );
        assert_eq!(receipts_and_unread(runtime), (1, 0));
        let message_id = runtime
            .read(|tree| tree.messages().keys().next().cloned().unwrap())
            .unwrap();
        let item = crate::sessions::queue::enqueue(&mut host.config.session.pending_queue, "next")
            .unwrap();
        host.config.session.record_prompt(item.text.clone());
        host.config.store.save(&mut host.config.session).unwrap();
        host.run_prompt(item.text.clone(), Some(item), false, None)
            .unwrap();
        assert_eq!(receipts_and_unread(runtime).0, 0);
        assert!(
            host.config
                .store
                .load_intents(&host.config.session.id)
                .unwrap()
                .submissions
                .iter()
                .any(|submission| {
                    runtime
                        .read(|tree| {
                            tree.command(&submission.command_id)
                                .is_some_and(|batch| batch.message_ids.contains(&message_id))
                        })
                        .unwrap()
                }),
            "the restored envelope rides the next prompt"
        );
    });
}

/// A cancelled turn closed its exchange before its terminal was
/// published: its receipts retire at publication, so neither the next
/// prompt nor a reopen settles them and the mail is not delivered again.
#[test]
fn a_cancelled_turn_with_a_closed_exchange_retires_its_waits_for_good() {
    with_unread_result(|host, runtime| {
        use plakous::sessions::feed::{FeedCancelCause, FeedFact, FeedRecord, FeedTurnOutcome};
        wait_outside_history(host, runtime);
        // The live publication is presentation, not settlement authority.
        // Seed the committed cancellation this topology-only fixture's
        // synthetic TurnHostOutcome represents.
        let session = host.opened.locator().session_id();
        runtime.mutate_then_emit(false, |tree| {
            let seq = tree.feed_cursor(&session.to_string()) + 1;
            let header = serde_json::from_value(serde_json::json!({
                "session_id": session, "seq": seq,
                "branch_id": host.opened.locator().branch_id(), "committed_at_ms": seq,
                "change": {"advanced": {"parent": host.opened.locator().checkpoint_id(),
                    "checkpoint": host.opened.locator().checkpoint_id(), "revision": seq, "head": "quiescent"}}
            })).unwrap();
            let record = FeedRecord::from_store(header, vec![FeedFact::TurnTerminal {
                turn_id: "00000000-0000-4000-8000-000000000001".parse().unwrap(),
                command_id: plakous::agent::CommandId::new("fixture:cancelled").unwrap(),
                outcome: FeedTurnOutcome::Cancelled { cause: FeedCancelCause::Requested },
                closed_exchange: true, steering_settled_unapplied: false,
                usage: plakous::agent::TurnUsage::default(),
            }]);
            tree.project_feed("root", &session.to_string(), &[record], &crate::agent_tree::TreeFeedReads::default())
                .map(|_| ()).map_err(|error| error.to_string())
        }).unwrap();
        host.publish_terminal(
            Ok(TurnHostOutcome::Interrupted {
                assistant: String::new(),
                exchange: TerminalExchange::Closed,
            }),
            Instant::now(),
            TerminalErrorPresentation::Events,
        );
        assert_eq!(receipts_and_unread(runtime), (0, 0));
        host.executor
            .block_on(host.opened.detach())
            .unwrap()
            .unwrap();
        let root = host.config.product_home.parent().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root);
        config.state_roots = host.config.state_roots.clone();
        config.store = host.config.store.clone();
        config.session = config.store.load(&host.config.session.id).unwrap();
        config.runtime_factory = host.config.runtime_factory.clone();
        let mut reopened = HostLoop::open(config, host.actions, host.events.clone(), None).unwrap();
        assert_eq!(receipts_and_unread(runtime), (0, 0));
        assert!(
            runtime
                .read(|tree| tree
                    .messages()
                    .values()
                    .all(|mail| mail.durable == MailDurableState::Acknowledged))
                .unwrap()
        );
        reopened
            .executor
            .block_on(reopened.opened.detach())
            .unwrap()
            .unwrap();
        host.opened = reopened.opened;
        host.config.session = reopened.config.session;
    });
}
