#![cfg(test)]

use super::*;

#[test]
#[ignore = "manual tree-fold microbenchmark"]
fn fold_microbench_twenty_agents() {
    for due in [false, true] {
        let (_tmp, mut tree) = tree();
        tree.grants_mut_for_test("root").max_depth = 2;
        tree.grants_mut_for_test("root").max_nodes = 32;
        let mut agents = Vec::new();
        for _ in 0..4 {
            let parent = tree
                .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                .unwrap()
                .agent_id;
            for _ in 0..5 {
                let child = tree
                    .spawn_agent(&parent, &spawn_req(), None)
                    .unwrap()
                    .agent_id;
                tree.nodes.get_mut(&child).unwrap().status = NodeStatus::Interrupted;
                agents.push(child);
            }
        }
        let mut base_feed = Feed::new(CHILD_SESSION);
        for _ in 0..500 {
            base_feed.push(Vec::new());
        }
        let mut feeds = Vec::new();
        for (index, agent) in agents.iter().enumerate() {
            let session = uuid(0x100 + index as u64);
            tree.allocate_session_for_test(
                agent,
                format!("{session}/{}/{}", uuid(0xb), uuid(0x3001)),
            )
            .unwrap();
            let records: Vec<_> = base_feed
                .records
                .iter()
                .map(|record| {
                    let mut header = *record.header();
                    header.session_id = session.parse().unwrap();
                    FeedRecord::from_store(header, Vec::new())
                })
                .collect();
            feeds.push((session, records));
            for ordinal in 0..200 {
                let id = format!("bench:{agent}:{ordinal}");
                let request = serde_json::to_string(&serde_json::json!({
                    "attempt_id": tree.node(agent).unwrap().attempt_id,
                    "decision": {"Interrupt": {"effective": false}},
                }))
                .unwrap();
                tree.operations.insert(
                    id.clone(),
                    OperationRecord {
                        fold: Some(FoldAnchor {
                            seq: if due { ordinal * 2 + 1 } else { 1000 + ordinal },
                            order: ordinal + 1,
                        }),
                        operation_id: id,
                        caller_agent_id: agent.clone(),
                        tool_name: operator_intents::INTERRUPT_KIND.to_owned(),
                        request_digest: digest_text(&request),
                        canonical_request: request,
                        terminal_code: None,
                        terminal_response: Some("effects_reserved".to_owned()),
                    },
                );
            }
        }
        let clone_start = std::time::Instant::now();
        for _ in 0..100 {
            std::hint::black_box(tree.clone());
        }
        let clone_ms = clone_start.elapsed().as_secs_f64() * 1000.0;
        let start = std::time::Instant::now();
        for (agent, (session, records)) in agents.iter().zip(&feeds) {
            tree.project_feed(agent, session, records, &TreeFeedReads::default())
                .unwrap();
        }
        eprintln!(
            "fold_microbench agents=20 records=500 intents=200 due={due} replay_ms={:.3} clones_100_ms={clone_ms:.3}",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
}

#[test]
fn repeatedly_closing_an_already_closed_subtree_adds_no_intents() {
    let (_tmp, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.close_agent("root", &close_req(&child), None).unwrap();
    let operations = tree.operations.clone();
    for _ in 0..10 {
        tree.close_agent("root", &close_req(&child), None).unwrap();
    }
    assert_eq!(tree.operations, operations);
}

#[test]
fn startup_folds_a_crash_window_finalize_and_settles_its_seal_once() {
    use crate::hub::TopologyHub;
    use plakous_workspace::occupancy_vcs::fake::FakeOp;
    let (_tmp, vcs, mut tree) = fake_tree();
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.record_child_finalization(
        &child.agent_id,
        "ended",
        ChildFinalizationDecision::Finalize,
        true,
        Some(ChildTerminal {
            attempt_id: child.attempt_id.clone(),
            turn_id: "ended".to_owned(),
            committed_at_ms: 1000,
            seq: 0,
            outcome: FeedTurnOutcome::Completed {
                finish: FinishReason::Stop,
            },
        }),
    )
    .unwrap();
    let store = Arc::new(MemoryTreeStore::new());
    tree.acknowledge_revision(store.save(&tree).unwrap());
    let id = tree.tree_id.clone();
    let loaded = ProductAgentTree::reopen(store.load(&id).unwrap().unwrap());
    let runtime = TopologyHub::new()
        .insert_root(&id, loaded, store.clone(), vcs.clone(), None)
        .unwrap();
    runtime
        .read(|tree| {
            assert_eq!(
                tree.attempt(&child.agent_id).unwrap().status,
                AttemptStatus::CompletedWithoutSubmission
            );
            assert_eq!(tree.state, TreeAdmissionState::Live);
            assert!(
                !tree
                    .operations
                    .values()
                    .any(|op| op.tool_name == "vcs_fault")
            );
            assert!(!tree.owes_effects());
        })
        .unwrap();
    assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
    let loaded = ProductAgentTree::reopen(store.load(&id).unwrap().unwrap());
    let runtime = TopologyHub::new()
        .insert_root(&id, loaded, store, vcs.clone(), None)
        .unwrap();
    assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
    runtime
        .read(|tree| {
            assert!(
                !tree
                    .operations
                    .values()
                    .any(|op| op.tool_name == "vcs_fault")
            );
            assert_ne!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::RecoveryRequired
            );
        })
        .unwrap();
}

#[test]
fn fold_due_does_not_parse_another_agents_malformed_close() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    tree.record_close_target(&child, 1000).unwrap();
    let close = tree
        .operations
        .values_mut()
        .find(|operation| operation.tool_name == operator_intents::CLOSE_KIND)
        .unwrap();
    close.canonical_request = "not json".to_owned();
    assert!(tree.fold_due("root").is_ok());
    assert!(tree.fold_due(&child).is_err());
}

#[test]
fn malformed_fenced_intent_keeps_operation_inputs_on_refusal() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    tree.record_close_target(&child, 1000).unwrap();
    let close = tree
        .operations
        .values_mut()
        .find(|operation| operation.tool_name == operator_intents::CLOSE_KIND)
        .unwrap();
    close.canonical_request = "not json".to_owned();
    close.terminal_response = Some("effects_reserved".to_owned());
    let operations = tree.operations.clone();
    let node = tree.node(&child).unwrap().clone();
    let attempt = tree.attempt(&child).unwrap().clone();
    assert!(tree.fold_due(&child).is_err());
    assert_eq!(tree.operations, operations);
    assert_eq!(tree.node(&child).unwrap(), &node);
    assert_eq!(tree.attempt(&child).unwrap(), &attempt);
}

#[test]
fn fold_due_leaves_another_agents_due_close_untouched() {
    let (_tmp, mut tree) = tree();
    let (child, _) = running_child(&mut tree);
    tree.record_close_target(&child, 1000).unwrap();
    let before = tree.node(&child).unwrap().clone();
    let attempt = tree.attempt(&child).unwrap().clone();
    tree.fold_due("root").unwrap();
    assert_eq!(tree.node(&child).unwrap(), &before);
    assert_eq!(tree.attempt(&child).unwrap(), &attempt);
    tree.fold_due(&child).unwrap();
    assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
}

#[test]
#[ignore = "manual close-agent microbenchmark"]
fn close_microbench_twenty_agents() {
    let (_tmp, mut tree) = tree();
    tree.grants_mut_for_test("root").max_depth = 2;
    tree.grants_mut_for_test("root").max_nodes = 32;
    let mut parents = Vec::new();
    let mut agents = Vec::new();
    for _ in 0..4 {
        let parent = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap()
            .agent_id;
        parents.push(parent.clone());
        agents.push(parent.clone());
        for _ in 0..4 {
            let child = tree
                .spawn_agent(&parent, &spawn_req(), None)
                .unwrap()
                .agent_id;
            tree.nodes.get_mut(&child).unwrap().status = NodeStatus::Interrupted;
            agents.push(child);
        }
    }
    assert_eq!(agents.len(), 20);
    for (index, agent) in agents.iter().enumerate() {
        let session = uuid(0x200 + index as u64);
        tree.allocate_session_for_test(agent, format!("{session}/{}/{}", uuid(0xb), uuid(0x3001)))
            .unwrap();
        let mut feed = Feed::new(CHILD_SESSION);
        for _ in 0..500 {
            feed.push(Vec::new());
        }
        let records: Vec<_> = feed
            .records
            .iter()
            .map(|record| {
                let mut header = *record.header();
                header.session_id = session.parse().unwrap();
                FeedRecord::from_store(header, Vec::new())
            })
            .collect();
        tree.project_feed(agent, &session, &records, &TreeFeedReads::default())
            .unwrap();
        for ordinal in 0..200 {
            let id = format!("close-bench:{agent}:{ordinal}");
            let request = serde_json::to_string(&serde_json::json!({
                "attempt_id": tree.node(agent).unwrap().attempt_id,
                "decision": {"Interrupt": {"effective": false}},
            }))
            .unwrap();
            tree.operations.insert(
                id.clone(),
                OperationRecord {
                    fold: Some(FoldAnchor {
                        seq: 1000 + ordinal,
                        order: ordinal + 1,
                    }),
                    operation_id: id,
                    caller_agent_id: agent.clone(),
                    tool_name: operator_intents::INTERRUPT_KIND.to_owned(),
                    request_digest: digest_text(&request),
                    canonical_request: request,
                    terminal_code: None,
                    terminal_response: Some("effects_reserved".into()),
                },
            );
        }
    }
    let mut times = Vec::new();
    for _ in 0..5 {
        let mut sample = tree.clone();
        let start = std::time::Instant::now();
        for parent in &parents {
            sample
                .close_agent("root", &close_req(parent), None)
                .unwrap();
        }
        times.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(
            agents
                .iter()
                .all(|agent| sample.node(agent).unwrap().status == NodeStatus::Closed)
        );
    }
    times.sort_by(f64::total_cmp);
    eprintln!(
        "close_microbench agents=20 records=500 intents=200 samples_ms={times:?} median_ms={:.3}",
        times[2]
    );
}
