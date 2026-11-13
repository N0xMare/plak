use super::*;
use crate::tree::test_support::*;
use crate::tree::{AgentCommandKind, CommandFact, TreeFeed, TreeFeedReads};

#[test]
fn dormant_tree_child_allocation_is_resolved_and_rebuilt_at_the_new_head() {
    let (_directory, mut source, child) = fidelity_source();
    let old = source.nodes[&child].session_locator.clone().unwrap();
    // An allocation is permanent across checkpoints on the same branch.
    let current = old.replace("000000000003", "000000000005");
    source.nodes.get_mut(&child).unwrap().session_locator = Some(current.clone());
    let marker_id = format!("child_session:{child}");
    let marker = &source.operations[&marker_id];
    assert!(marker.fold.is_none() && marker.terminal_response.is_none());
    let cut = DormantTree::capture(&source).unwrap();
    let cut: DormantTree = serde_json::from_slice(&serde_json::to_vec(&cut).unwrap()).unwrap();
    let next = current.replace("000000000005", "000000000006");
    let installed = cut
        .reconstruct(&BTreeMap::from([(current, next.clone())]))
        .unwrap()
        .0;
    assert_eq!(
        installed.nodes[&child].session_locator.as_ref(),
        Some(&next)
    );
    assert_eq!(installed.operations[&marker_id].canonical_request, next);
    assert_eq!(installed.operations[&marker_id].operation_id, marker_id);
    assert!(DormantTree::capture(&installed).is_ok());
}

#[test]
fn dormant_tree_child_allocation_mismatch_or_an_unrelated_unresolved_kind_refuses() {
    let (_directory, mut source, child) = fidelity_source();
    let marker_id = format!("child_session:{child}");
    let saved = source.operations[&marker_id].clone();
    let marker = source.operations.get_mut(&marker_id).unwrap();
    marker.canonical_request = marker
        .canonical_request
        .replace("000000000002", "000000000009");
    marker.request_digest = digest_text(&marker.canonical_request);
    assert!(
        DormantTree::capture(&source)
            .unwrap_err()
            .contains("disagrees")
    );
    source.operations.insert(marker_id.clone(), saved);
    source.operations.get_mut(&marker_id).unwrap().tool_name = "another_unresolved_kind".into();
    assert!(DormantTree::capture(&source).is_err());
}

#[test]
fn dormant_tree_fidelity_preserves_mail_decisions_lineage_attempts_and_retained_child_commands() {
    let (_directory, source, child) = fidelity_source();
    let locator = source.nodes[&child].session_locator.clone().unwrap();
    let session = locator.split('/').next().unwrap().to_owned();
    let owed = source.commands["owed"].clone();
    let order = source.nodes[&child].fold_order;
    let cut = DormantTree::capture(&source).unwrap();
    let bytes = serde_json::to_vec(&cut).unwrap();
    let decoded: DormantTree = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.messages, source.messages);
    assert!(!decoded.messages.is_empty());
    assert_eq!(decoded.forked_from, source.forked_from().cloned());
    assert!(!decoded.decisions.is_empty());
    assert_eq!(
        decoded
            .decisions
            .iter()
            .find(|decision| decision.operation_id == "decision")
            .unwrap()
            .operation()
            .unwrap(),
        source.operations["decision"]
    );
    let target = locator.replace("000000000003", "000000000004");
    let mut installed = decoded
        .reconstruct(&BTreeMap::from([(locator, target)]))
        .unwrap()
        .0;
    let attempts = installed.attempts.clone();
    let messages = installed.messages.clone();
    installed.clear_feed_views();
    assert_eq!(installed.attempts, attempts);
    assert_eq!(installed.messages, messages);
    assert_eq!(installed.commands["owed"], owed);
    assert!(!installed.command_behind_retention("owed"));
    // A child whose reader starts from zero is covered by the imported semantic
    // floor, not a retention loss that may refuse the still-owed command.
    installed.feed_cursors.insert(session.clone(), 0);
    TreeFeed {
        session_id: session,
        retained_from: 21,
        below_retained: Some(20),
        heads: vec![],
        head_turns: BTreeMap::new(),
        records: vec![],
        reads: TreeFeedReads::default(),
    }
    .apply_checked(&mut installed, &child)
    .unwrap();
    assert_eq!(installed.commands["owed"], owed);
    assert!(!installed.command_behind_retention("owed"));
    assert_eq!(installed.nodes[&child].fold_order, order);
    assert!(installed.take_effects().is_empty());
}

#[test]
fn dormant_tree_semantics_survive_empty_feed_refold_without_replaying_admissions() {
    let (_directory, mut source) = tree();
    let session = "00000000-0000-4000-8000-000000000001";
    source.feed_cursors.insert(session.into(), 20);
    let mut command = AgentCommand::intent(
        "accepted",
        "root",
        None,
        AgentCommandKind::RootBatch,
        "first",
        vec![],
    );
    command.fact = Some(CommandFact::Accepted);
    command.accepted_seq = Some(7);
    source
        .commands
        .insert(command.command_id.clone(), command.clone());
    let owed = AgentCommand::intent(
        "owed",
        "root",
        None,
        AgentCommandKind::RootBatch,
        "next",
        vec![],
    );
    source
        .commands
        .insert(owed.command_id.clone(), owed.clone());
    let document = DormantTree::capture(&source).unwrap();
    let json = serde_json::to_string(&document).unwrap();
    let decoded: DormantTree = serde_json::from_str(&json).unwrap();
    let mut installed = decoded.reconstruct(&BTreeMap::new()).unwrap().0;
    installed.clear_feed_views();
    assert_eq!(installed.feed_cursor(session), 20);
    assert_eq!(installed.commands.get("accepted"), Some(&command));
    assert_eq!(installed.commands.get("owed"), Some(&owed));
    TreeFeed {
        session_id: session.into(),
        retained_from: 21,
        below_retained: Some(20),
        heads: vec![],
        head_turns: BTreeMap::new(),
        records: vec![],
        reads: TreeFeedReads::default(),
    }
    .apply_checked(&mut installed, "root")
    .unwrap();
    assert_eq!(installed.commands.get("accepted"), Some(&command));
    assert_eq!(installed.commands.get("owed"), Some(&owed));
    assert!(installed.take_effects().is_empty());
    let mut unknown: serde_json::Value = serde_json::from_str(&json).unwrap();
    unknown["effect_journal"] = serde_json::json!([]);
    assert!(serde_json::from_value::<DormantTree>(unknown).is_err());
}

#[test]
fn dormant_tree_refuses_active_and_unjoined_source_operations() {
    let (_directory, mut source) = tree();
    source.nodes.get_mut("root").unwrap().active_turn = true;
    assert!(DormantTree::capture(&source).is_err());
    source.nodes.get_mut("root").unwrap().active_turn = false;
    source.operations.insert(
        "unresolved".into(),
        OperationRecord {
            operation_id: "unresolved".into(),
            caller_agent_id: "root".into(),
            tool_name: "adopt_candidate".into(),
            request_digest: digest_text("{}"),
            canonical_request: "{}".into(),
            fold: None,
            terminal_code: None,
            terminal_response: None,
        },
    );
    assert!(DormantTree::capture(&source).is_err());
}

fn fidelity_source() -> (tempfile::TempDir, ProductAgentTree, String) {
    let (directory, mut source) = tree();
    let child = source
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    let locator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003";
    let session = "00000000-0000-4000-8000-000000000001";
    let anchor = FoldAnchor { seq: 20, order: 7 };
    source.record_child_session(&child, locator).unwrap();
    source.nodes.get_mut(&child).unwrap().session_locator = Some(locator.into());
    source.nodes.get_mut(&child).unwrap().fold_order = Some(anchor.order);
    source.nodes.get_mut("root").unwrap().fold_order = Some(anchor.order);
    source.nodes.get_mut(&child).unwrap().status = NodeStatus::Completed;
    let attempt = source.nodes[&child].attempt_id.clone().unwrap();
    source.attempts.get_mut(&attempt).unwrap().status = AttemptStatus::Completed;
    source.attempts.get_mut(&attempt).unwrap().ended_at = Some(42);
    source.lineage.forked_from = Some(ForkedFrom::Import {
        provenance: "original tree provenance".into(),
    });
    source.feed_cursors.insert(session.into(), 20);
    source.messages.insert(
        "mail".into(),
        MailRecord {
            message_id: "mail".into(),
            from: "root".into(),
            to: child.clone(),
            body: "retained message".into(),
            purpose: crate::tree::MessagePurpose::Coordinate,
            priority: crate::tree::MessagePriority::Deferred,
            in_reply_to: None,
            disposition: crate::tree::MailDisposition::Queued,
            durable: crate::tree::MailDurableState::Accepted,
        },
    );
    let intent: super::super::operator_intents::OperatorIntent =
        serde_json::from_value(serde_json::json!({
            "attempt_id": attempt, "decision": {"Interrupt": {"effective": false, "cause": null}}
        }))
        .unwrap();
    let canonical_request = canonical_json(&intent).unwrap();
    source.operations.insert(
        "decision".into(),
        OperationRecord {
            operation_id: "decision".into(),
            caller_agent_id: child.clone(),
            tool_name: "interrupt".into(),
            request_digest: digest_text(&canonical_request),
            canonical_request,
            fold: Some(anchor),
            terminal_code: None,
            terminal_response: Some("effects_reserved".into()),
        },
    );
    let owed = AgentCommand::intent(
        "owed",
        &child,
        source.nodes[&child].attempt_id.clone(),
        AgentCommandKind::Wake,
        "next",
        vec![],
    );
    source.commands.insert("owed".into(), owed.clone());
    let _ = source.take_effects();
    (directory, source, child)
}
