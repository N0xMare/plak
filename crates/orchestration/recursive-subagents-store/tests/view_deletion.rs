//! Delete only `SQLite` view columns, retaining intents and external effects.
#![cfg(feature = "test-support")]
#![allow(clippy::unwrap_used, reason = "deterministic offline law fixtures")]

use std::sync::Arc;

use plakous::agent::{AbandonedTurnClaim, CommandId, TurnUsage};
use plakous::model::{FinishReason, HistorySegmentKind, TurnOrigin};
use plakous::sessions::feed::{
    FeedCommandKind, FeedFact, FeedHeader, FeedOperationKind, FeedOperationState, FeedRecord,
    FeedTurnOutcome,
};
use plakous_recursive_subagents::protocol::{
    AdoptCandidateRequest, AttemptScopedRequest, NodeStatus, SpawnAgentRequest, SpawnWorkspace,
    SubmitResultRequest, Tier, WriterMode,
};
use plakous_recursive_subagents::tree::turn_outcome::FailedTurnDisposition;
use plakous_recursive_subagents::tree::{
    AbandonedChildTurn, AgentCommand, AgentCommandKind, ChildFinalizationDecision, FakeVcs,
    MailDurableState, ProductAgentTree, RecoveryResolution, TerminalRead, TreeFeed, TreeFeedReads,
    TreeStore,
};
use plakous_recursive_subagents_store::SqliteTreeStore;
use plakous_workspace::occupancy_vcs::VcsError;
use serde_json::json;

const SESSION: &str = "00000000-0000-4000-8000-000000000100";
const TURN: &str = "00000000-0000-4000-8000-000000001000";

#[test]
fn late_recovery_read_rebuilds_at_the_closures_own_position() {
    assert_late_recovery_read(false, false, false);
}

#[test]
fn late_recovery_read_preserves_a_later_retirement_on_rebuild() {
    assert_late_recovery_read(true, false, false);
}

#[test]
fn retained_late_recovery_read_refolds_in_place_with_its_seed() {
    assert_late_recovery_read(true, true, false);
}

#[test]
fn refold_preserves_usage_for_terminals_beyond_the_previous_cursor() {
    assert_late_recovery_read(true, false, true);
    assert_late_recovery_read(true, true, true);
}

#[allow(
    clippy::too_many_lines,
    reason = "late-read law, SQLite clearing cut and both deletion paths"
)]
fn assert_late_recovery_read(later_retirement: bool, retained: bool, new_terminal: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let (mut live, vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    let mut feed = vec![record(1, accepted_fact("start"))];
    live.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
        .unwrap();
    live.record_host_turn_failure(
        child,
        FailedTurnDisposition::RecoveryRequired,
        "original recovery obligation".into(),
        Vec::new(),
    )
    .unwrap();
    live.fold_due(child).unwrap();
    let mut terminal = terminal_fact("start", false);
    if let FeedFact::TurnTerminal {
        outcome,
        closed_exchange,
        ..
    } = &mut terminal
    {
        *outcome = FeedTurnOutcome::RecoveryRequired;
        *closed_exchange = false;
    }
    feed.push(record(2, terminal));
    let history = FeedFact::HistoryAppended {
        turn_id: Some(TURN.parse().unwrap()),
        segment: HistorySegmentKind::ToolExchange,
        first_ordinal: 0,
        count: 1,
        first_entry: TURN.parse().unwrap(),
        last_entry: TURN.parse().unwrap(),
        tool_results: Vec::new(),
    };
    let closure = record(3, history.clone());
    feed.push(FeedRecord::from_store(
        *closure.header(),
        vec![
            history,
            FeedFact::OperationAdvanced {
                operation_id: 7,
                turn_id: Some(TURN.parse().unwrap()),
                kind: FeedOperationKind::ProcessSession { job_number: None },
                state: FeedOperationState::Finalized,
            },
        ],
    ));
    live.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
        .unwrap();
    assert_eq!(
        live.node(child).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    if later_retirement {
        live.record_host_retirement(child, "later release failed".into())
            .unwrap();
        live.fold_due(child).unwrap();
    }
    let mut reads = TreeFeedReads::default();
    reads.insert_terminal(
        TURN.to_owned(),
        TerminalRead {
            message: "closure abandonment".into(),
            abandoned: Some(AbandonedChildTurn {
                operations: vec![7],
                claim: AbandonedTurnClaim::RunningTool,
            }),
        },
    );
    if later_retirement {
        let before = live.to_persisted();
        assert!(
            live.project_feed(child, SESSION, &feed[2..], &reads)
                .is_err(),
            "an incomplete late-read page must refuse without clearing live views"
        );
        assert_eq!(format!("{:?}", live.to_persisted()), format!("{before:?}"));
    }
    let new_turn = "00000000-0000-4000-8000-000000001001";
    if new_terminal {
        live.record_command(AgentCommand::intent(
            "new-start",
            child,
            Some(spawned.attempt_id.clone()),
            AgentCommandKind::Start,
            "new turn",
            Vec::new(),
        ))
        .unwrap();
        let mut accepted = accepted_fact("new-start");
        if let FeedFact::CommandAccepted {
            turn_id, sequence, ..
        } = &mut accepted
        {
            *turn_id = Some(new_turn.parse().unwrap());
            *sequence = 4;
        }
        feed.push(record(4, accepted));
        let mut terminal = terminal_fact("new-start", false);
        if let FeedFact::TurnTerminal { turn_id, usage, .. } = &mut terminal {
            *turn_id = new_turn.parse().unwrap();
            *usage = TurnUsage::new(plakous::model::Usage::new(11, 7, 0, 0, 0), 1, 1, 3).unwrap();
        }
        feed.push(record(5, terminal));
    }
    let retained_page = if retained {
        let head = live
            .node(child)
            .unwrap()
            .session_locator
            .as_ref()
            .unwrap()
            .parse()
            .unwrap();
        Some(TreeFeed {
            session_id: SESSION.to_owned(),
            retained_from: 2,
            below_retained: Some(1),
            heads: vec![head],
            head_turns: [(head, Some(TURN.parse().unwrap()))].into(),
            records: feed[1..].to_vec(),
            reads: reads.clone(),
        })
    } else {
        None
    };
    let apply = |tree: &mut ProductAgentTree| {
        if let Some(page) = &retained_page {
            page.apply(tree, child).unwrap()
        } else {
            tree.project_feed(child, SESSION, &feed, &reads).unwrap()
        }
    };
    let late = apply(&mut live);
    assert_eq!(
        late.applied,
        if new_terminal { 2 } else { 0 },
        "only new records count after a refold"
    );
    assert_eq!(late.terminal_usages.len(), usize::from(new_terminal));
    if new_terminal {
        assert_eq!(late.terminal_usages[0].turn_id.to_string(), new_turn);
        assert_eq!(
            late.terminal_usages[0].usage,
            TurnUsage::new(plakous::model::Usage::new(11, 7, 0, 0, 0), 1, 1, 3).unwrap()
        );
    }
    assert!(live.node(child).unwrap().fold_order.is_some());
    if later_retirement {
        assert_eq!(
            live.node(child).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(
            live.attempt(child).unwrap().failure.as_ref().unwrap().kind,
            "turn_recovery_required"
        );
    }
    let revision = live.revision;
    let node = live.node(child).unwrap().clone();
    let attempt = live.attempt(child).unwrap().clone();
    apply(&mut live);
    assert_eq!(
        live.revision, revision,
        "rereading an applied closure never refolds"
    );
    assert_eq!(live.node(child).unwrap(), &node);
    assert_eq!(live.attempt(child).unwrap(), &attempt);
    let database = tmp.path().join("late-in-place.sqlite3");
    let store = SqliteTreeStore::open(&database).unwrap();
    let _ = store.save(&live).unwrap();
    let mut cold = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
    let before = cold.revision;
    apply(&mut cold);
    assert_eq!(
        cold.revision, before,
        "the applied-read view survives reopen"
    );
    assert_eq!(settle_law_effects(&mut live, &vcs), 1);
    if let Some(page) = &retained_page {
        let expected = law_rows(&live);
        for sql in [false, true] {
            let mut rebuilt = if sql {
                let database = tmp.path().join("retained-delete.sqlite3");
                let store = SqliteTreeStore::open(&database).unwrap();
                let mut unpublished = live.clone();
                unpublished.stored_revision = 0;
                let _ = store.save(&unpublished).unwrap();
                drop(store);
                delete_views(&database);
                let store = SqliteTreeStore::open(&database).unwrap();
                ProductAgentTree::reopen(store.load("tree").unwrap().unwrap())
            } else {
                let mut rebuilt = live.clone();
                rebuilt.clear_feed_views();
                rebuilt
            };
            page.apply(&mut rebuilt, child).unwrap();
            rebuilt
                .project_feed(
                    "root",
                    &rebuilt.tree_id.clone(),
                    &[],
                    &TreeFeedReads::default(),
                )
                .unwrap();
            assert_eq!(law_rows(&rebuilt), expected, "retained sql={sql}");
            assert!(rebuilt.take_effects().is_empty());
        }
    } else {
        assert_exact_single_session_rebuild(&live, child, &feed, &reads);
    }
}

#[test]
fn two_abandonment_reads_on_one_attempt_are_inert_on_every_reread() {
    use plakous::sessions::feed::FeedCancelCause;
    let tmp = tempfile::tempdir().unwrap();
    let (mut live, vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    let mut feed = Vec::new();
    let mut reads = TreeFeedReads::default();
    for (index, turn) in [TURN, "00000000-0000-4000-8000-000000001001"]
        .iter()
        .enumerate()
    {
        let command = if index == 0 { "start" } else { "second-start" };
        if index != 0 {
            live.record_command(AgentCommand::intent(
                command,
                child,
                Some(spawned.attempt_id.clone()),
                AgentCommandKind::Start,
                "second turn",
                Vec::new(),
            ))
            .unwrap();
        }
        let mut accepted = accepted_fact(command);
        if let FeedFact::CommandAccepted {
            turn_id, sequence, ..
        } = &mut accepted
        {
            *turn_id = Some(turn.parse().unwrap());
            *sequence = (index * 2 + 1) as u64;
        }
        feed.push(record((index * 2 + 1) as u64, accepted));
        let mut terminal = terminal_fact(command, false);
        if let FeedFact::TurnTerminal {
            turn_id, outcome, ..
        } = &mut terminal
        {
            *turn_id = turn.parse().unwrap();
            *outcome = FeedTurnOutcome::Cancelled {
                cause: FeedCancelCause::Abandoned {
                    claim: AbandonedTurnClaim::RunningTool,
                    cause: plakous::agent::AbandonmentCause::ProcessLost,
                    model_round_completed: true,
                    tool_round_completed: false,
                },
            };
        }
        feed.push(record((index * 2 + 2) as u64, terminal));
        reads.insert_terminal(
            (*turn).to_owned(),
            TerminalRead {
                message: format!("abandoned turn {index}"),
                abandoned: Some(AbandonedChildTurn {
                    operations: Vec::new(),
                    claim: AbandonedTurnClaim::RunningTool,
                }),
            },
        );
        live.project_feed(child, SESSION, &feed, &reads).unwrap();
        settle_law_effects(&mut live, &vcs);
        assert_eq!(live.attempt(child).unwrap().attempt_id, spawned.attempt_id);
    }
    live.record_host_retirement(child, "later release failed".into())
        .unwrap();
    live.fold_due(child).unwrap();
    live.project_feed(child, SESSION, &feed, &reads).unwrap();
    let revision = live.revision;
    let expected = law_rows(&live);
    live.project_feed(child, SESSION, &feed, &reads).unwrap();
    assert_eq!(
        live.revision, revision,
        "the second reread cannot refold an earlier applied read"
    );
    assert_eq!(law_rows(&live), expected);
    let store = SqliteTreeStore::open(&tmp.path().join("two-reads.sqlite3")).unwrap();
    let _ = store.save(&live).unwrap();
    let mut cold = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
    let revision = cold.revision;
    cold.project_feed(child, SESSION, &feed, &reads).unwrap();
    assert_eq!(cold.revision, revision);
    assert_exact_single_session_rebuild(&live, child, &feed, &reads);
}

#[test]
fn same_cursor_intents_after_an_intent_reserved_seal_rebuild_exactly() {
    for first in [0, 1, 3] {
        for interrupt in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let (mut live, vcs, spawned) = spawned_tree(tmp.path());
            let child = &spawned.agent_id;
            let mut feed = vec![record(1, accepted_fact("start"))];
            live.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
                .unwrap();
            record_decision(&mut live, child, &mut feed, first);
            if !interrupt {
                // A submitted intent can already be in flight when the seal's
                // terminating intent is recorded but not yet applied.
                live.submit_result(
                    child,
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: r#"{"ok":true}"#.into(),
                    },
                    None,
                )
                .unwrap();
            }
            live.fold_due(child).unwrap();
            assert_eq!(live.node(child).unwrap().status, NodeStatus::Sealing);
            let seals = live.take_effects();
            assert_eq!(seals.seals.len(), 1);
            let anchor = live.feed_cursor(SESSION);
            if interrupt {
                live.bind_turn_owner(child, TURN.to_owned());
                live.interrupt_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: child.clone(),
                        expected_attempt_id: Some(spawned.attempt_id.clone()),
                    },
                    None,
                )
                .unwrap();
            } else if first == 1 {
                live.record_host_cancellation(child).unwrap();
            } else {
                live.record_host_materialization(child).unwrap();
            }
            live.fold_due(child).unwrap();
            assert_eq!(live.feed_cursor(SESSION), anchor);
            assert_eq!(live.node(child).unwrap().status, NodeStatus::Sealing);
            assert!(live.take_effects().is_empty());
            let expected = law_rows(&live);
            for sql in [false, true] {
                let mut rebuilt = if sql {
                    let database = tmp.path().join("pending.sqlite3");
                    let store = SqliteTreeStore::open(&database).unwrap();
                    let _ = store.save(&live).unwrap();
                    drop(store);
                    delete_views(&database);
                    let store = SqliteTreeStore::open(&database).unwrap();
                    // Isolate the fold from reopen's intentional orphan-seal fault.
                    store.load("tree").unwrap().unwrap().rows().clone()
                } else {
                    let mut rebuilt = live.clone();
                    rebuilt.clear_feed_views();
                    rebuilt
                };
                rebuilt
                    .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
                    .unwrap();
                rebuilt
                    .project_feed(
                        "root",
                        &rebuilt.tree_id.clone(),
                        &[],
                        &TreeFeedReads::default(),
                    )
                    .unwrap();
                assert_eq!(
                    law_rows(&rebuilt),
                    expected,
                    "first={first} interrupt={interrupt} sql={sql}"
                );
                assert!(rebuilt.take_effects().is_empty());
                rebuilt
                    .project_feed(child, SESSION, &[], &TreeFeedReads::default())
                    .unwrap();
                assert_eq!(law_rows(&rebuilt), expected);
                assert!(rebuilt.take_effects().is_empty());
            }
            let _ = live.complete(seals.run(&*vcs));
            let _ = settle_law_effects(&mut live, &vcs);
            assert_exact_single_session_rebuild(&live, child, &feed, &TreeFeedReads::default());
        }
    }
}

#[test]
fn projection_lag_intents_rebuild_at_the_cursor_not_the_committed_tail() {
    for decision in 0..9 {
        let tmp = tempfile::tempdir().unwrap();
        let (mut live, vcs, spawned) = spawned_tree(tmp.path());
        let child = &spawned.agent_id;
        let mut feed = vec![record(1, accepted_fact("start"))];
        live.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        // The session has committed its terminal, but the tree has not seen it.
        feed.push(record(2, terminal_fact("start", false)));
        match decision {
            0..=2 | 5 => record_decision(&mut live, child, &mut Vec::new(), decision),
            3 => live
                .record_host_turn_failure(
                    child,
                    FailedTurnDisposition::RecoveryRequired,
                    "lagged host recovery".into(),
                    Vec::new(),
                )
                .unwrap(),
            4 => {
                live.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: child.clone(),
                        expected_attempt_id: Some(spawned.attempt_id.clone()),
                    },
                    None,
                )
                .unwrap();
            }
            6 => {
                live.interrupt_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: child.clone(),
                        expected_attempt_id: Some(spawned.attempt_id.clone()),
                    },
                    None,
                )
                .unwrap();
            }
            7 => {
                live.submit_result(
                    child,
                    &SubmitResultRequest {
                        turn_token: live.attempt(child).unwrap().turn_token,
                        output_json: r#"{"ok":true}"#.into(),
                    },
                    None,
                )
                .unwrap();
            }
            _ => {
                let mut scratch = live.clone();
                let terminal = scratch
                    .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
                    .unwrap()
                    .ended_turns
                    .into_iter()
                    .next()
                    .unwrap()
                    .1;
                live.record_child_finalization(
                    child,
                    TURN,
                    ChildFinalizationDecision::Finalize,
                    true,
                    Some(terminal),
                )
                .unwrap();
            }
        }
        assert_eq!(
            live.feed_cursor(SESSION),
            1,
            "recording must not consume the committed tail"
        );
        assert!(
            live.operations()
                .values()
                .filter_map(|op| op.fold)
                .all(|anchor| anchor.seq == 1)
        );
        live.fold_due(child).unwrap();
        let mut seals = settle_law_effects(&mut live, &vcs);
        live.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        seals += settle_law_effects(&mut live, &vcs);
        assert!(seals <= 1, "decision={decision}");
        assert_exact_single_session_rebuild(&live, child, &feed, &TreeFeedReads::default());
    }
}

fn settle_law_effects(tree: &mut ProductAgentTree, vcs: &FakeVcs) -> usize {
    let mut seals = 0;
    loop {
        let effects = tree.take_effects();
        if effects.is_empty() {
            return seals;
        }
        seals += effects.seals.len();
        let _ = tree.complete(effects.run(vcs));
    }
}

fn accepted_fact(command: &str) -> FeedFact {
    FeedFact::CommandAccepted {
        command_id: CommandId::new(command).unwrap(),
        kind: FeedCommandKind::Root {
            origin: TurnOrigin::UserPrompt,
        },
        turn_id: Some(TURN.parse().unwrap()),
        sequence: 1,
        operation_id: None,
    }
}

fn assert_exact_single_session_rebuild(
    live: &ProductAgentTree,
    child: &str,
    feed: &[FeedRecord],
    reads: &TreeFeedReads,
) {
    let expected = law_rows(live);
    for sql in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let mut rebuilt = if sql {
            let database = tmp.path().join("law.sqlite3");
            let store = SqliteTreeStore::open(&database).unwrap();
            let mut unpublished = live.clone();
            unpublished.stored_revision = 0;
            let _ = store.save(&unpublished).unwrap();
            drop(store);
            delete_views(&database);
            let store = SqliteTreeStore::open(&database).unwrap();
            ProductAgentTree::reopen(store.load(&live.tree_id).unwrap().unwrap())
        } else {
            let mut rebuilt = live.clone();
            rebuilt.clear_feed_views();
            rebuilt
        };
        for page in feed.chunks(1) {
            rebuilt.project_feed(child, SESSION, page, reads).unwrap();
        }
        rebuilt
            .project_feed(
                "root",
                &rebuilt.tree_id.clone(),
                &[],
                &TreeFeedReads::default(),
            )
            .unwrap();
        assert_eq!(law_rows(&rebuilt), expected, "sql={sql}");
        assert!(rebuilt.take_effects().is_empty());
        rebuilt.project_feed(child, SESSION, feed, reads).unwrap();
        if rebuilt.node(child).unwrap().fold_order.is_none() {
            assert!(rebuilt.take_effects().is_empty());
            rebuilt.project_feed(child, SESSION, feed, reads).unwrap();
            rebuilt
                .project_feed(
                    "root",
                    &rebuilt.tree_id.clone(),
                    &[],
                    &TreeFeedReads::default(),
                )
                .unwrap();
        }
        assert_eq!(law_rows(&rebuilt), expected);
        assert!(rebuilt.take_effects().is_empty());
    }
}

#[test]
fn generated_multi_child_cascades_rebuild_in_both_deletion_paths() {
    // Fixed seed and bounded cases keep this in the parallel lane.
    let mut seed = 0x4c_2026_u64;
    for case in 0..12 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        assert_multi_child_cascade(case % 3, usize::try_from(seed % 256).unwrap());
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "multi-session deletion law and first-application oracle"
)]
fn assert_multi_child_cascade(kind: usize, choices: usize) {
    let tmp = tempfile::tempdir().unwrap();
    let mut base = ProductAgentTree::bootstrap_root_for_test(
        "multi-child",
        "root",
        "owner",
        tmp.path().to_path_buf(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut snapshot = base.snapshot.clone();
    snapshot.max_depth = 2;
    base.install_snapshot(snapshot);
    let parent = base
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("orchestrate", Tier::ReadOnly, "parent", "delegate"),
            None,
        )
        .unwrap();
    let first = base
        .spawn_agent(&parent.agent_id, &SpawnAgentRequest::test_review(), None)
        .unwrap();
    let second = base
        .spawn_agent(&parent.agent_id, &SpawnAgentRequest::test_review(), None)
        .unwrap();
    let agents = [parent, first, second];
    let sessions = [
        SESSION,
        "00000000-0000-4000-8000-000000000101",
        "00000000-0000-4000-8000-000000000102",
    ];
    let mut feeds = Vec::new();
    for (index, agent) in agents.iter().enumerate() {
        base.record_child_session(
            &agent.agent_id,
            &format!(
                "{}/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001",
                sessions[index]
            ),
        )
        .unwrap();
        let command = format!("start-{index}");
        base.record_command(AgentCommand::intent(
            &command,
            &agent.agent_id,
            Some(agent.attempt_id.clone()),
            AgentCommandKind::Start,
            "task",
            Vec::new(),
        ))
        .unwrap();
        let accepted = record_for_session(
            sessions[index],
            1,
            FeedFact::CommandAccepted {
                command_id: CommandId::new(&command).unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(TURN.parse().unwrap()),
                sequence: 1,
                operation_id: None,
            },
        );
        base.project_feed(
            &agent.agent_id,
            sessions[index],
            std::slice::from_ref(&accepted),
            &TreeFeedReads::default(),
        )
        .unwrap();
        feeds.push(vec![accepted]);
    }
    for index in 1..3 {
        if choices & (1 << index) != 0 {
            feeds[index].push(record_for_session(
                sessions[index],
                2,
                terminal_fact(&format!("start-{index}"), false),
            ));
            let projection = base
                .project_feed(
                    &agents[index].agent_id,
                    sessions[index],
                    &feeds[index],
                    &TreeFeedReads::default(),
                )
                .unwrap();
            base.record_child_finalization(
                &agents[index].agent_id,
                TURN,
                ChildFinalizationDecision::Finalize,
                true,
                projection
                    .ended_turns
                    .into_iter()
                    .next()
                    .map(|(_, terminal)| terminal),
            )
            .unwrap();
            base.fold_due(&agents[index].agent_id).unwrap();
        }
    }
    match kind {
        0 => feeds[0].push(record_for_session(
            sessions[0],
            2,
            terminal_fact("start-0", true),
        )),
        1 => base.record_host_cancellation(&agents[0].agent_id).unwrap(),
        _ => {
            feeds[0].push(record_for_session(
                sessions[0],
                2,
                terminal_fact("start-0", false),
            ));
            // Capture terminal evidence without applying the cascade to base.
            let mut scratch = base.clone();
            let projection = scratch
                .project_feed(
                    &agents[0].agent_id,
                    sessions[0],
                    &feeds[0],
                    &TreeFeedReads::default(),
                )
                .unwrap();
            base.record_child_finalization(
                &agents[0].agent_id,
                TURN,
                ChildFinalizationDecision::Finalize,
                true,
                projection
                    .ended_turns
                    .into_iter()
                    .next()
                    .map(|(_, terminal)| terminal),
            )
            .unwrap();
        }
    }
    let mut live = ProductAgentTree::reopen(base.to_persisted());
    live.project_feed(
        &agents[0].agent_id,
        sessions[0],
        &feeds[0],
        &TreeFeedReads::default(),
    )
    .unwrap();
    assert!(live.take_effects().is_empty());
    let expected = law_rows(&live);
    let orders = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [2, 0, 1],
        [1, 2, 0],
        [2, 1, 0],
    ];
    for first_application in [false, true] {
        for sql in [false, true] {
            let source = if first_application { &base } else { &live };
            let mut deleted = if sql {
                let database = tmp.path().join(format!("law-{first_application}.sqlite3"));
                let store = SqliteTreeStore::open(&database).unwrap();
                let mut unpublished = source.clone();
                unpublished.stored_revision = 0;
                let _ = store.save(&unpublished).unwrap();
                drop(store);
                delete_views(&database);
                let store = SqliteTreeStore::open(&database).unwrap();
                ProductAgentTree::reopen(store.load("multi-child").unwrap().unwrap())
            } else {
                let mut cleared = source.clone();
                cleared.clear_feed_views();
                ProductAgentTree::reopen(cleared.to_persisted())
            };
            let order = orders[choices % orders.len()];
            for index in order {
                let before = law_rows(&deleted);
                let result = deleted.project_feed(
                    &agents[index].agent_id,
                    sessions[index],
                    &feeds[index],
                    &TreeFeedReads::default(),
                );
                if result.is_err() {
                    assert!(first_application && index == 0);
                    assert_eq!(law_rows(&deleted), before, "retry must not advance parent");
                    assert!(deleted.take_effects().is_empty());
                }
            }
            deleted
                .project_feed(
                    &agents[0].agent_id,
                    sessions[0],
                    &feeds[0],
                    &TreeFeedReads::default(),
                )
                .unwrap();
            deleted
                .project_feed(
                    "root",
                    &deleted.tree_id.clone(),
                    &[],
                    &TreeFeedReads::default(),
                )
                .unwrap();
            if first_application {
                assert_eq!(
                    first_application_rows(&deleted),
                    first_application_rows(&live),
                    "kind={kind} sql={sql} order={order:?}"
                );
            } else {
                assert_eq!(
                    law_rows(&deleted),
                    expected,
                    "kind={kind} sql={sql} order={order:?}"
                );
            }
            assert!(deleted.take_effects().is_empty());
            let own_expected = law_rows(&deleted);
            for index in order {
                deleted
                    .project_feed(
                        &agents[index].agent_id,
                        sessions[index],
                        &feeds[index],
                        &TreeFeedReads::default(),
                    )
                    .unwrap();
            }
            assert_eq!(law_rows(&deleted), own_expected);
            assert!(deleted.take_effects().is_empty());
            for second_sql in [false, true] {
                let mut twice = if second_sql {
                    let database = tmp
                        .path()
                        .join(format!("twice-{first_application}-{sql}.sqlite3"));
                    let store = SqliteTreeStore::open(&database).unwrap();
                    let mut unpublished = deleted.clone();
                    unpublished.stored_revision = 0;
                    let _ = store.save(&unpublished).unwrap();
                    drop(store);
                    delete_views(&database);
                    let store = SqliteTreeStore::open(&database).unwrap();
                    ProductAgentTree::reopen(store.load("multi-child").unwrap().unwrap())
                } else {
                    let mut twice = deleted.clone();
                    twice.clear_feed_views();
                    twice
                };
                for index in order {
                    twice
                        .project_feed(
                            &agents[index].agent_id,
                            sessions[index],
                            &feeds[index],
                            &TreeFeedReads::default(),
                        )
                        .unwrap();
                }
                twice
                    .project_feed(
                        "root",
                        &twice.tree_id.clone(),
                        &[],
                        &TreeFeedReads::default(),
                    )
                    .unwrap();
                assert_eq!(
                    law_rows(&twice),
                    own_expected,
                    "exact own-run rebuild, sql={second_sql}"
                );
                assert!(twice.take_effects().is_empty());
            }
        }
    }
}

fn law_rows(tree: &ProductAgentTree) -> String {
    let persisted = tree.to_persisted();
    let rows = persisted.rows();
    format!(
        "{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
        rows.nodes(),
        rows.attempts(),
        rows.workspaces(),
        rows.messages(),
        rows.operations(),
        rows.commands(),
        rows.feed_cursors()
    )
}

fn first_application_rows(tree: &ProductAgentTree) -> String {
    let mut nodes = tree.nodes().clone();
    for node in nodes.values_mut() {
        node.fold_order = None;
    }
    // Only freshly minted target close identities vary between first applications.
    let operations: std::collections::BTreeMap<_, _> = tree
        .operations()
        .values()
        .map(|operation| {
            let mut operation = operation.clone();
            if operation.tool_name == "close" {
                operation.operation_id = format!("close:{}", operation.caller_agent_id);
                operation.fold.as_mut().unwrap().order = 0;
            }
            (operation.operation_id.clone(), operation)
        })
        .collect();
    format!(
        "{nodes:?}\n{:?}\n{:?}\n{:?}\n{operations:?}\n{:?}\n{:?}",
        tree.attempts(),
        tree.workspaces(),
        tree.messages(),
        tree.commands(),
        tree.feed_cursors()
    )
}

fn terminal_fact(command: &str, failed: bool) -> FeedFact {
    FeedFact::TurnTerminal {
        command_id: CommandId::new(command).unwrap(),
        turn_id: TURN.parse().unwrap(),
        outcome: if failed {
            FeedTurnOutcome::Failed {
                kind: plakous::agent::AgentErrorKind::Internal,
                model_kind: None,
                context: None,
            }
        } else {
            FeedTurnOutcome::Completed {
                finish: FinishReason::Stop,
            }
        },
        usage: TurnUsage::default(),
        closed_exchange: true,
        steering_settled_unapplied: false,
    }
}

fn record_for_session(session: &str, seq: u64, fact: FeedFact) -> FeedRecord {
    let original = record(seq, fact.clone());
    let mut header = *original.header();
    header.session_id = session.parse().unwrap();
    FeedRecord::from_store(header, vec![fact])
}

#[test]
fn attempt_candidate_view_is_deleted_and_rebuilt_in_memory() {
    assert_candidate_view_rebuild(false);
}

#[test]
fn sqlite_attempt_candidate_view_is_deleted_and_rebuilt() {
    assert_candidate_view_rebuild(true);
}

fn assert_candidate_view_rebuild(sql: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let database = tmp.path().join("subagents.sqlite3");
    let (mut tree, vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    let feed = vec![record(
        1,
        FeedFact::CommandAccepted {
            command_id: CommandId::new("start").unwrap(),
            kind: FeedCommandKind::Root {
                origin: TurnOrigin::UserPrompt,
            },
            turn_id: Some(TURN.parse().unwrap()),
            sequence: 1,
            operation_id: None,
        },
    )];
    tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
        .unwrap();
    tree.record_host_cancellation(child).unwrap();
    tree.fold_due(child).unwrap();
    let effects = tree.take_effects();
    let _ = tree.complete(effects.run(&*vcs));
    let expected = tree.attempt(child).unwrap().clone();
    assert!(expected.candidate_id.is_some(), "successful divergent seal");
    let candidate = expected.candidate_id.clone().unwrap();
    if sql {
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let deleted = store.load("tree").unwrap().unwrap();
        assert!(
            deleted
                .rows()
                .attempt(child)
                .unwrap()
                .candidate_id
                .is_none()
        );
        tree = ProductAgentTree::reopen(deleted);
    } else {
        tree.clear_feed_views();
        assert!(tree.attempt(child).unwrap().candidate_id.is_none());
    }
    assert!(
        tree.candidates().contains_key(&candidate),
        "external candidate survives deletion"
    );
    tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.attempt(child).unwrap(), &expected);
    assert!(tree.take_effects().is_empty());
    tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.attempt(child).unwrap(), &expected);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "opening and withdrawal cuts share one SQLite fixture"
)]
fn sqlite_followup_open_save_and_withdrawal_cuts_rebuild_remaining_inputs() {
    use plakous_recursive_subagents::protocol::{
        AttemptScopedRequest, MessagePurpose, SendAgentMessageRequest,
    };
    use plakous_recursive_subagents::tools::TreeDriver;
    use std::sync::Mutex;

    let tmp = tempfile::tempdir().unwrap();
    let database = tmp.path().join("subagents.sqlite3");
    let (mut tree, vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    tree.set_active_turn(child, true).unwrap();
    tree.interrupt_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: child.clone(),
            expected_attempt_id: Some(spawned.attempt_id.clone()),
        },
        None,
    )
    .unwrap();
    tree.set_active_turn(child, false).unwrap();
    let expected_previous = tree.attempt(child).unwrap().clone();
    let opener = tree
        .send_agent_message(
            "root",
            &SendAgentMessageRequest {
                agent_id: child.clone(),
                message: "again".into(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: Some(r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"next":{"type":"boolean"}},"required":["next"],"additionalProperties":false}"#.to_owned()),
            },
            Some("withdraw-send"),
        )
        .unwrap()
        .message_id;
    let next_attempt = tree.attempt(child).unwrap().attempt_id.clone();
    tree.record_command(AgentCommand::intent(
        "followup-start",
        child,
        Some(next_attempt.clone()),
        AgentCommandKind::Followup,
        "again",
        vec![opener.clone()],
    ))
    .unwrap();
    let expected_opened = tree.attempt(child).unwrap().clone();
    let expected_superseded = tree.attempts()[&spawned.attempt_id].clone();
    let store = Arc::new(SqliteTreeStore::open(&database).unwrap());
    tree.acknowledge_revision(store.save(&tree).unwrap());
    delete_views(&database);
    let saved = store.load("tree").unwrap().unwrap();
    assert_eq!(
        saved.rows().node(child).unwrap().attempt_id.as_ref(),
        Some(&spawned.attempt_id)
    );
    let mut rebuilt = ProductAgentTree::reopen(saved);
    rebuilt
        .project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(rebuilt.attempt(child).unwrap(), &expected_opened);
    assert_eq!(
        rebuilt.node(child).unwrap().output_schema_json,
        expected_opened.output_schema_json
    );
    assert_eq!(
        rebuilt.node(child).unwrap().schema_digest,
        expected_opened.schema_digest
    );
    assert_eq!(rebuilt.attempts()[&spawned.attempt_id], expected_superseded);
    rebuilt.acknowledge_revision(store.save(&rebuilt).unwrap());
    let driver = TreeDriver::new(Arc::new(Mutex::new(rebuilt)), store.clone(), vcs, None);
    // The fixture has an authoritative empty runtime tail; no Start was sent.
    driver
        .withdraw_unstarted_followup(
            child,
            &opener,
            SESSION,
            0,
            Some((
                "withdraw-send".to_owned(),
                plakous_recursive_subagents::protocol::TopologyErrorCode::Capacity,
            )),
        )
        .unwrap();
    let saved = store.load("tree").unwrap().unwrap();
    assert!(!saved.rows().attempts().contains_key(&next_attempt));
    assert_eq!(
        saved.rows().mail(&opener).unwrap().durable,
        MailDurableState::Acknowledged
    );
    assert_eq!(
        saved.rows().operations()["withdraw-send"].terminal_code,
        Some(plakous_recursive_subagents::protocol::TopologyErrorCode::Capacity)
    );
    assert!(
        !saved
            .rows()
            .commands()
            .values()
            .any(|command| command.attempt_id.as_ref() == Some(&next_attempt))
    );
    delete_views(&database);
    let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
    rebuilt
        .project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(rebuilt.attempt(child).unwrap(), &expected_previous);
    assert_eq!(
        rebuilt.node(child).unwrap().output_schema_json,
        expected_previous.output_schema_json
    );
    assert!(rebuilt.take_effects().is_empty());
    rebuilt
        .project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert!(
        !rebuilt
            .unread_mail(child)
            .any(|mail| mail.message_id == opener)
    );
    rebuilt.resume_tree().unwrap();
    let later = rebuilt
        .send_agent_message(
            "root",
            &SendAgentMessageRequest {
                agent_id: child.clone(),
                message: "later Start".into(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
    assert!(
        rebuilt
            .pending_mail(child)
            .iter()
            .all(|mail| mail.message_id == later.message_id)
    );
    assert_eq!(
        rebuilt.mail(&opener).unwrap().durable,
        MailDurableState::Acknowledged
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one pruned-result law over both deletion paths"
)]
fn pruned_closed_attempt_result_is_not_pushed_again_after_view_deletion() {
    use plakous_recursive_subagents::protocol::{
        AttemptScopedRequest, MessagePurpose, SendAgentMessageRequest,
    };
    for sql in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            "pruned-result",
            "root",
            "owner",
            tmp.path().to_path_buf(),
            "tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        let sender = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        tree.record_child_session(&child.agent_id, &format!("{SESSION}/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001")).unwrap();
        tree.record_command(AgentCommand::intent(
            "start",
            &child.agent_id,
            Some(child.attempt_id.clone()),
            AgentCommandKind::Start,
            "task",
            Vec::new(),
        ))
        .unwrap();
        let feed = vec![
            record(
                1,
                FeedFact::CommandAccepted {
                    command_id: CommandId::new("start").unwrap(),
                    kind: FeedCommandKind::Root {
                        origin: TurnOrigin::UserPrompt,
                    },
                    turn_id: Some(TURN.parse().unwrap()),
                    sequence: 1,
                    operation_id: None,
                },
            ),
            record(
                2,
                FeedFact::TurnTerminal {
                    command_id: CommandId::new("start").unwrap(),
                    turn_id: TURN.parse().unwrap(),
                    outcome: FeedTurnOutcome::Failed {
                        kind: plakous::agent::AgentErrorKind::Internal,
                        model_kind: None,
                        context: None,
                    },
                    usage: TurnUsage::default(),
                    closed_exchange: true,
                    steering_settled_unapplied: false,
                },
            ),
        ];
        tree.project_feed(&child.agent_id, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        let result = tree
            .messages()
            .values()
            .find(|mail| mail.purpose == MessagePurpose::Result)
            .unwrap()
            .message_id
            .clone();
        tree.acknowledge_mail(&result).unwrap();
        tree.close_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: child.agent_id.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap();
        for index in 0..65 {
            let mail = tree
                .send_agent_message(
                    &sender.agent_id,
                    &SendAgentMessageRequest {
                        agent_id: "root".to_owned(),
                        message: format!("later {index}"),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap();
            tree.acknowledge_mail(&mail.message_id).unwrap();
        }
        assert!(
            tree.mail(&result).is_none(),
            "the result witness really was pruned"
        );
        let node = tree.node(&child.agent_id).unwrap().clone();
        let attempt = tree.attempt(&child.agent_id).unwrap().clone();
        let messages = tree.messages().clone();
        let operations = tree.operations().clone();
        if sql {
            let store = SqliteTreeStore::open(&database).unwrap();
            let _ = store.save(&tree).unwrap();
            drop(store);
            delete_views(&database);
            let store = SqliteTreeStore::open(&database).unwrap();
            tree = ProductAgentTree::reopen(store.load("pruned-result").unwrap().unwrap());
        } else {
            tree.clear_feed_views();
        }
        tree.project_feed(&sender.agent_id, SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        tree.project_feed(&child.agent_id, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(tree.node(&child.agent_id).unwrap(), &node);
        assert_eq!(tree.attempt(&child.agent_id).unwrap(), &attempt);
        assert_eq!(
            tree.messages(),
            &messages,
            "replay must not emit another result"
        );
        assert_eq!(tree.operations(), &operations);
        assert!(tree.take_effects().is_empty());
        tree.project_feed(&child.agent_id, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(tree.messages(), &messages);
    }
}

#[test]
fn sqlite_pending_close_seal_rebuilds_its_as_of_view_and_orphan_fault() {
    let tmp = tempfile::tempdir().unwrap();
    let database = tmp.path().join("subagents.sqlite3");
    let (mut tree, _vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    tree.close_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: child.clone(),
            expected_attempt_id: Some(spawned.attempt_id.clone()),
        },
        None,
    )
    .unwrap();
    assert_eq!(tree.node(child).unwrap().status, NodeStatus::Sealing);
    let expected_node = tree.node(child).unwrap().clone();
    let expected_attempt = tree.attempt(child).unwrap().clone();
    let store = SqliteTreeStore::open(&database).unwrap();
    let _ = store.save(&tree).unwrap();
    tree.clear_feed_views();
    tree.project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.node(child).unwrap(), &expected_node);
    assert_eq!(tree.attempt(child).unwrap(), &expected_attempt);
    let mut reopened = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
    reopened.fold_due(child).unwrap();
    let expected_node = reopened.node(child).unwrap().clone();
    let expected_attempt = reopened.attempt(child).unwrap().clone();
    assert_eq!(expected_node.status, NodeStatus::RecoveryRequired);
    let _ = store.save(&reopened).unwrap();
    drop(store);
    delete_views(&database);
    let store = SqliteTreeStore::open(&database).unwrap();
    let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
    rebuilt
        .project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
    assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
    assert!(rebuilt.take_effects().is_empty());
    let revision = rebuilt.revision;
    rebuilt
        .project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(rebuilt.revision, revision);
}

#[test]
fn sqlite_submitted_rebuilds_output_and_status_from_its_intent() {
    let tmp = tempfile::tempdir().unwrap();
    let database = tmp.path().join("subagents.sqlite3");
    let (mut tree, _vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    tree.fold_due(child).unwrap();
    tree.submit_result(
        child,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    let expected_attempt = tree.attempt(child).unwrap().clone();
    let expected_node = tree.node(child).unwrap().clone();
    let store = SqliteTreeStore::open(&database).unwrap();
    let _ = store.save(&tree).unwrap();
    tree.clear_feed_views();
    tree.project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.attempt(child).unwrap(), &expected_attempt);
    assert_eq!(tree.node(child).unwrap(), &expected_node);
    assert!(tree.take_effects().is_empty());
    drop(store);
    delete_views(&database);
    let store = SqliteTreeStore::open(&database).unwrap();
    let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
    rebuilt
        .project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
    assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
    assert_eq!(
        rebuilt
            .node(child)
            .unwrap()
            .presentation_status(rebuilt.state),
        NodeStatus::Paused
    );
    assert!(rebuilt.take_effects().is_empty());
    let revision = rebuilt.revision;
    rebuilt
        .project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(rebuilt.revision, revision);
}

#[test]
fn sqlite_close_survives_view_deletion_without_live_inputs() {
    for shape in 0..5 {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let (mut tree, _vcs, spawned) = spawned_tree_with_session(tmp.path(), false);
        let child = &spawned.agent_id;
        if shape == 1 {
            tree.reserve_turn_owner(child);
        }
        if shape == 2 {
            assert!(tree.begin_seal(child));
        }
        if shape >= 3 {
            tree.record_host_turn_failure(
                child,
                FailedTurnDisposition::RecoveryRequired,
                "release uncertain".into(),
                Vec::new(),
            )
            .unwrap();
            tree.fold_due(child).unwrap();
        }
        if shape == 4 {
            tree.record_terminal_recovery_refusal(
                child,
                &spawned.attempt_id,
                "recovery_session_unavailable",
                "incompatible child session",
            );
        }
        tree.close_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: child.clone(),
                expected_attempt_id: Some(spawned.attempt_id.clone()),
            },
            None,
        )
        .unwrap();
        tree.settle_for_test();
        let expected_attempt = tree.attempt(child).unwrap().clone();
        let expected_node = tree.node(child).unwrap().clone();
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        tree.clear_feed_views();
        tree.project_feed(child, SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(tree.attempt(child).unwrap(), &expected_attempt);
        assert_eq!(tree.node(child).unwrap(), &expected_node);
        assert!(tree.take_effects().is_empty());
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        rebuilt
            .project_feed(child, SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
        assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
        assert!(rebuilt.take_effects().is_empty());
        let revision = rebuilt.revision;
        rebuilt
            .project_feed(child, SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(rebuilt.revision, revision);
    }
}

#[test]
fn sqlite_recovery_abandonment_survives_view_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let database = tmp.path().join("subagents.sqlite3");
    let (mut tree, _vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    tree.record_host_turn_failure(
        child,
        FailedTurnDisposition::RecoveryRequired,
        "release uncertain".into(),
        Vec::new(),
    )
    .unwrap();
    tree.fold_due(child).unwrap();
    assert!(tree.abandon_recovery_for_delete(child).unwrap());
    assert!(!tree.abandon_recovery_for_delete(child).unwrap());
    let expected_attempt = tree.attempt(child).unwrap().clone();
    let expected_node = tree.node(child).unwrap().clone();
    let _ = tree.take_effects();
    let store = SqliteTreeStore::open(&database).unwrap();
    let _ = store.save(&tree).unwrap();
    tree.clear_feed_views();
    tree.project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(tree.attempt(child).unwrap(), &expected_attempt);
    assert_eq!(tree.node(child).unwrap(), &expected_node);
    drop(store);
    delete_views(&database);
    let store = SqliteTreeStore::open(&database).unwrap();
    let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
    rebuilt
        .project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
    assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
    assert!(rebuilt.fault_reason(child).is_none());
    assert!(rebuilt.take_effects().is_empty());
    let revision = rebuilt.revision;
    rebuilt
        .project_feed(child, SESSION, &[], &TreeFeedReads::default())
        .unwrap();
    assert_eq!(rebuilt.revision, revision);
    let _ = store.save(&rebuilt).unwrap();
    let mut twice = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
    twice.fold_due(child).unwrap();
    assert_eq!(twice.attempt(child).unwrap(), &expected_attempt);
    assert_eq!(twice.node(child).unwrap(), &expected_node);
    assert!(twice.take_effects().is_empty());
}

#[test]
fn sqlite_interrupt_survives_view_deletion_without_live_ownership() {
    for effective in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let (mut tree, _vcs, spawned) = spawned_tree(tmp.path());
        let child = &spawned.agent_id;
        tree.fold_due(child).unwrap();
        if effective {
            tree.reserve_turn_owner(child);
        }
        tree.interrupt_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: child.clone(),
                expected_attempt_id: Some(spawned.attempt_id.clone()),
            },
            Some("interrupt"),
        )
        .unwrap();
        let expected = tree.node(child).unwrap().clone();
        assert_eq!(
            expected.status,
            if effective {
                NodeStatus::Interrupted
            } else {
                NodeStatus::Running
            }
        );
        let expected = ProductAgentTree::reopen(tree.to_persisted())
            .node(child)
            .unwrap()
            .clone();
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        rebuilt
            .project_feed(child, SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(rebuilt.node(child).unwrap(), &expected);
        let once = rebuilt.node(child).unwrap().clone();
        rebuilt
            .project_feed(child, SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(rebuilt.node(child).unwrap(), &once);
        assert!(rebuilt.take_effects().is_empty());
    }
}

#[test]
fn sqlite_spawn_adopt_and_cleanup_outcomes_survive_view_deletion() {
    for outcome in 0..3 {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let (mut tree, vcs, spawned) = spawned_tree_with_session(tmp.path(), outcome == 1);
        let child = &spawned.agent_id;
        let mut feed = Vec::new();
        if outcome == 1 {
            feed.push(record(
                1,
                FeedFact::CommandAccepted {
                    command_id: CommandId::new("start").unwrap(),
                    kind: FeedCommandKind::Root {
                        origin: TurnOrigin::UserPrompt,
                    },
                    turn_id: Some(TURN.parse().unwrap()),
                    sequence: 1,
                    operation_id: None,
                },
            ));
            tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
                .unwrap();
            tree.record_host_cancellation(child).unwrap();
            tree.fold_due(child).unwrap();
            let effects = tree.take_effects();
            let _ = tree.complete(effects.run(&*vcs));
            let candidate = tree.attempt(child).unwrap().candidate_id.clone().unwrap();
            tree.begin_adopt(
                "root",
                &AdoptCandidateRequest {
                    candidate_id: candidate,
                    expected_target: None,
                    evidence: None,
                },
            )
            .unwrap();
        } else {
            let request =
                SpawnAgentRequest::test_named("implement", Tier::Mutate, "worker", "task");
            tree.fail_spawn_refused("root", &request, None, child, "account refused");
            if outcome == 2 {
                vcs.unmint_next_with(Err(VcsError::mint_failed("removal refused")));
            }
        }
        let effects = tree.take_effects();
        let _ = tree.complete(effects.run(&*vcs));
        let expected_attempt = tree.attempt(child).unwrap().clone();
        let expected_node = tree.node(child).unwrap().clone();
        let mut expected_fault = tree.fault_reason(child).map(str::to_owned);
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        tree.clear_feed_views();
        tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(tree.attempt(child).unwrap(), &expected_attempt);
        assert_eq!(tree.node(child).unwrap(), &expected_node);
        assert!(tree.take_effects().is_empty());
        let mut reopened = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        reopened
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        let expected_attempt = reopened.attempt(child).unwrap().clone();
        let expected_node = reopened.node(child).unwrap().clone();
        if outcome == 2 {
            expected_fault = reopened.fault_reason(child).map(str::to_owned);
        }
        let _ = store.save(&reopened).unwrap();
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        rebuilt
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
        assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
        assert_eq!(rebuilt.fault_reason(child), expected_fault.as_deref());
        assert!(rebuilt.take_effects().is_empty());
        let once = rebuilt.revision;
        rebuilt
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(rebuilt.revision, once);
        assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
        assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
        let _ = store.save(&rebuilt).unwrap();
        let mut again = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        again
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(again.attempt(child).unwrap(), &expected_attempt);
        assert!(again.take_effects().is_empty());
    }
}

#[test]
fn sqlite_vcs_fault_captures_prior_diagnostic_and_replays_its_settlement() {
    for settled in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let (mut tree, vcs, spawned) = spawned_tree(tmp.path());
        let child = &spawned.agent_id;
        let feed = vec![record(
            1,
            FeedFact::CommandAccepted {
                command_id: CommandId::new("start").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(TURN.parse().unwrap()),
                sequence: 1,
                operation_id: None,
            },
        )];
        tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        tree.record_host_cancellation(child).unwrap();
        tree.fold_due(child).unwrap();
        let effects = tree.take_effects();
        let _ = tree.complete(effects.run(&*vcs));
        tree.begin_adopt(
            "root",
            &AdoptCandidateRequest {
                candidate_id: tree.attempt(child).unwrap().candidate_id.clone().unwrap(),
                expected_target: None,
                evidence: None,
            },
        )
        .unwrap();
        tree.record_fault(child, "earlier precise diagnostic");
        tree.restore_paused();
        assert_eq!(tree.fault_reason(child), Some("earlier precise diagnostic"));
        if settled {
            let effects = tree.take_effects();
            let _ = tree.complete(effects.run(&*vcs));
        }
        let expected_attempt = tree.attempt(child).unwrap().clone();
        let expected_node = tree.node(child).unwrap().clone();
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        rebuilt
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
        assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
        assert_eq!(
            rebuilt.fault_reason(child),
            Some("earlier precise diagnostic")
        );
        assert!(rebuilt.take_effects().is_empty());
        let _ = store.save(&rebuilt).unwrap();
        let mut again = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        again
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(again.attempt(child).unwrap(), &expected_attempt);
        assert_eq!(again.node(child).unwrap(), &expected_node);
        assert!(again.take_effects().is_empty());
    }
}

#[test]
fn sqlite_fold_metadata_round_trips_and_rejects_invalid_positions() {
    let tmp = tempfile::tempdir().unwrap();
    let database = tmp.path().join("subagents.sqlite3");
    let (mut tree, _vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    tree.project_feed(
        child,
        SESSION,
        &[record(
            1,
            FeedFact::CommandAccepted {
                command_id: CommandId::new("start").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(TURN.parse().unwrap()),
                sequence: 1,
                operation_id: None,
            },
        )],
        &TreeFeedReads::default(),
    )
    .unwrap();
    let store = SqliteTreeStore::open(&database).unwrap();
    let _ = store.save(&tree).unwrap();
    let mut cold = store.load("tree").unwrap().unwrap().rows().clone();
    let order = cold.revision;
    cold.record_host_retirement(child, "release uncertain".into())
        .unwrap();
    let intent = cold
        .operations()
        .values()
        .find(|operation| operation.tool_name == "host_decision")
        .unwrap();
    assert_eq!(
        intent.fold,
        Some(plakous_recursive_subagents::tree::FoldAnchor { seq: 1, order })
    );
    let _ = store.save(&cold).unwrap();
    let loaded = store.load("tree").unwrap().unwrap();
    assert_eq!(loaded.rows().operations(), cold.operations());
    assert_eq!(loaded.rows().nodes(), cold.nodes());
    let db = rusqlite::Connection::open(&database).unwrap();
    let anchor: (i64, i64, String, String) = db
        .query_row(
            "SELECT fold_anchor,fold_order,typeof(fold_anchor),typeof(fold_order) FROM agent_operations WHERE tool_name='host_decision'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        anchor,
        (
            1,
            i64::try_from(order).unwrap(),
            "integer".into(),
            "integer".into()
        )
    );
    for invalid in [
        "UPDATE agent_operations SET fold_anchor=NULL WHERE tool_name='host_decision'",
        "UPDATE agent_operations SET fold_order=NULL WHERE tool_name='host_decision'",
        "UPDATE agent_operations SET fold_anchor=-1 WHERE tool_name='host_decision'",
        "UPDATE agent_operations SET fold_order=0 WHERE tool_name='host_decision'",
        "UPDATE agent_nodes SET fold_order=-1",
    ] {
        assert!(db.execute(invalid, []).is_err(), "{invalid}");
    }
    db.execute("UPDATE agent_nodes SET fold_order=7", [])
        .unwrap();
    let mut loaded = store.load("tree").unwrap().unwrap().rows().clone();
    assert!(
        loaded
            .nodes()
            .values()
            .all(|node| node.fold_order == Some(7))
    );
    let _ = store.save(&loaded).unwrap();
    assert_eq!(
        store.load("tree").unwrap().unwrap().rows().nodes(),
        loaded.nodes()
    );
    loaded.clear_feed_views();
    assert!(
        loaded
            .nodes()
            .values()
            .all(|node| node.fold_order.is_none())
    );
    delete_views(&database);
    assert!(
        store
            .load("tree")
            .unwrap()
            .unwrap()
            .rows()
            .nodes()
            .values()
            .all(|node| node.fold_order.is_none())
    );
}

#[test]
fn sqlite_bindings_use_plain_turn_ids_and_integer_anchors() {
    let tmp = tempfile::tempdir().unwrap();
    let database = tmp.path().join("subagents.sqlite3");
    let (mut tree, _vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    let accepted = record(
        1,
        FeedFact::CommandAccepted {
            command_id: CommandId::new("start").unwrap(),
            kind: FeedCommandKind::Root {
                origin: TurnOrigin::UserPrompt,
            },
            turn_id: Some(TURN.parse().unwrap()),
            sequence: 1,
            operation_id: None,
        },
    );
    tree.project_feed(child, SESSION, &[accepted], &TreeFeedReads::default())
        .unwrap();
    let store = SqliteTreeStore::open(&database).unwrap();
    let _ = store.save(&tree).unwrap();
    let db = rusqlite::Connection::open(&database).unwrap();
    let command: (String, i64, String) = db
        .query_row(
            "SELECT turn_id,accepted_seq,typeof(accepted_seq) FROM agent_commands WHERE command_id='start'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(command, (TURN.to_owned(), 1, "integer".to_owned()));
    let attempt: String = db
        .query_row(
            "SELECT turn_id FROM agent_attempts WHERE agent_id=?1",
            [child],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attempt, TURN);
    assert!(
        db.execute("UPDATE agent_commands SET accepted_seq=-1", [])
            .is_err()
    );
    let loaded = store.load("tree").unwrap().unwrap();
    assert_eq!(loaded.rows().commands(), tree.commands());
    assert_eq!(loaded.rows().attempts(), tree.attempts());
    tree = loaded.rows().clone();

    let head = tree
        .node(child)
        .unwrap()
        .session_locator
        .as_ref()
        .unwrap()
        .parse()
        .unwrap();
    tree.clear_feed_views();
    TreeFeed {
        session_id: SESSION.to_owned(),
        retained_from: 30,
        below_retained: Some(29),
        heads: vec![head],
        head_turns: std::collections::BTreeMap::from([(head, Some(TURN.parse().unwrap()))]),
        records: Vec::new(),
        reads: TreeFeedReads::default(),
    }
    .apply(&mut tree, child)
    .unwrap();
    let _ = store.save(&tree).unwrap();
    let seeded: (String, i64, String) = db
        .query_row(
            "SELECT a.turn_id,n.fold_order,typeof(n.fold_order) FROM agent_attempts a JOIN agent_nodes n USING(tree_id,agent_id) WHERE a.agent_id=?1",
            [child],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(seeded, (TURN.to_owned(), 0, "integer".to_owned()));
    assert_eq!(tree.feed_cursor(SESSION), 29);
    assert!(
        db.execute("UPDATE agent_nodes SET fold_order=-1", [])
            .is_err()
    );
    let loaded = store.load("tree").unwrap().unwrap();
    assert_eq!(loaded.rows().commands(), tree.commands());
    assert_eq!(loaded.rows().attempts(), tree.attempts());
}

fn record(seq: u64, fact: FeedFact) -> FeedRecord {
    let checkpoint = format!("00000000-0000-4000-8000-{:012x}", 0x3000 + seq);
    let change = if seq == 1 {
        json!({"created": {"genesis": checkpoint, "revision": seq}})
    } else {
        json!({"advanced": {"parent": "00000000-0000-4000-8000-000000003001",
            "checkpoint": checkpoint, "revision": seq, "head": "quiescent"}})
    };
    let header: FeedHeader = serde_json::from_value(json!({
        "session_id": SESSION, "seq": seq,
        "branch_id": "00000000-0000-4000-8000-00000000000b",
        "committed_at_ms": 1000 * seq, "change": change,
    }))
    .unwrap();
    FeedRecord::from_store(header, vec![fact])
}

#[test]
fn sqlite_view_deletion_reopens_host_intents_without_repeating_effects() {
    for decision in 0..7 {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let (mut tree, vcs, spawned) = spawned_tree(tmp.path());
        let child = &spawned.agent_id;
        let mut feed = vec![record(
            1,
            FeedFact::CommandAccepted {
                command_id: CommandId::new("start").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(TURN.parse().unwrap()),
                sequence: 1,
                operation_id: None,
            },
        )];
        tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        record_decision(
            &mut tree,
            child,
            &mut feed,
            if decision == 6 { 5 } else { decision },
        );
        tree.fold_due(child).unwrap();
        if decision == 4 {
            tree.record_host_recovery(
                child,
                &spawned.attempt_id,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "released".into(),
                },
            )
            .unwrap();
            tree.fold_due(child).unwrap();
        }
        let effects = tree.take_effects();
        assert_eq!(effects.seals.len(), usize::from(decision != 2));
        if decision == 5 {
            vcs.seal_next_with(Ok(FakeVcs::base_for(child)));
        }
        if decision == 6 {
            vcs.seal_next_with(Err(VcsError::seal_failed("snapshot refused")));
        }
        let _ = tree.complete(effects.run(&*vcs));
        if decision == 5 {
            assert!(tree.attempt(child).unwrap().candidate_id.is_none());
        }
        let expected_attempt = tree.attempt(child).unwrap().clone();
        let expected_node = tree.node(child).unwrap().clone();
        let expected_command = tree.command("start").unwrap().clone();
        let expected_cursor = tree.feed_cursor(SESSION);
        let expected_mail = tree.messages().clone();
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        rebuilt
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(
            rebuilt.attempt(child).unwrap(),
            &expected_attempt,
            "decision={decision}"
        );
        assert_eq!(
            rebuilt.node(child).unwrap(),
            &expected_node,
            "decision={decision}"
        );
        assert_eq!(rebuilt.command("start").unwrap(), &expected_command);
        assert_eq!(rebuilt.feed_cursor(SESSION), expected_cursor);
        assert_eq!(rebuilt.messages(), &expected_mail);
        assert!(rebuilt.take_effects().seals.is_empty());
        rebuilt
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
        assert!(rebuilt.take_effects().seals.is_empty());
    }
}

#[test]
fn sqlite_orphaned_seals_rebuild_finalize_and_submitted_cancellation_once() {
    for decision in [0, 3] {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let (mut tree, vcs, spawned) = spawned_tree(tmp.path());
        let child = &spawned.agent_id;
        let mut feed = vec![record(
            1,
            FeedFact::CommandAccepted {
                command_id: CommandId::new("start").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(TURN.parse().unwrap()),
                sequence: 1,
                operation_id: None,
            },
        )];
        tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        tree.submit_result(
            child,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        record_decision(&mut tree, child, &mut feed, decision);
        tree.fold_due(child).unwrap();
        assert_eq!(tree.take_effects().seals.len(), 1);
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        let mut reopened = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        reopened
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(
            reopened.attempt(child).unwrap().status,
            plakous_recursive_subagents::protocol::AttemptStatus::Completed
        );
        let expected_attempt = reopened.attempt(child).unwrap().clone();
        let expected_node = reopened.node(child).unwrap().clone();
        let expected_operations = reopened.operations().clone();
        reopened.acknowledge_revision(store.save(&reopened).unwrap());
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        rebuilt.set_vcs(vcs);
        for _ in 0..2 {
            rebuilt
                .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
                .unwrap();
            assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
            assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
            assert_eq!(rebuilt.operations(), &expected_operations);
            assert!(rebuilt.take_effects().is_empty());
        }
        rebuilt.acknowledge_revision(store.save(&rebuilt).unwrap());
        let mut again = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        again
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(again.attempt(child).unwrap(), &expected_attempt);
        assert_eq!(again.node(child).unwrap(), &expected_node);
        assert_eq!(again.operations(), &expected_operations);
        assert!(again.take_effects().is_empty());
    }
}

#[test]
fn both_deletion_paths_reset_mail_and_return_receipts_and_rebuild_identically() {
    use plakous_recursive_subagents::protocol::{MessagePurpose, SendAgentMessageRequest};
    let tmp = tempfile::tempdir().unwrap();
    let database = tmp.path().join("parity.sqlite3");
    let (mut live, vcs, spawned) = spawned_tree(tmp.path());
    let child = &spawned.agent_id;
    let mut ids = Vec::new();
    for text in ["accepted", "returned", "undeliverable"] {
        ids.push(
            live.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: child.clone(),
                    message: text.to_owned(),
                    purpose: Some(MessagePurpose::Coordinate),
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
            .message_id,
        );
    }
    for (command, id) in ["accepted", "returned"].iter().zip(&ids) {
        live.record_command(AgentCommand::intent(
            *command,
            child,
            Some(spawned.attempt_id.clone()),
            AgentCommandKind::MailStart,
            *command,
            vec![id.clone()],
        ))
        .unwrap();
    }
    let mut terminal = terminal_fact("returned", true);
    if let FeedFact::TurnTerminal {
        outcome: FeedTurnOutcome::Failed { context, .. },
        ..
    } = &mut terminal
    {
        *context = Some(
            serde_json::from_value(json!({"withheld":true,"remedy":"resend_shorter"})).unwrap(),
        );
    }
    let feed = vec![
        record(1, accepted_fact("accepted")),
        record(2, accepted_fact("returned")),
        record(3, terminal),
    ];
    live.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
        .unwrap();
    live.record_undeliverable_mail(&ids[2]).unwrap();
    live.project_mail_decisions(child).unwrap();
    let _ = settle_law_effects(&mut live, &vcs);
    assert!(live.admitted_mail_returned("returned"));
    assert_eq!(
        live.mail(&ids[0]).unwrap().durable,
        MailDurableState::Acknowledged
    );
    assert_eq!(
        live.mail(&ids[2]).unwrap().durable,
        MailDurableState::Acknowledged
    );
    let store = SqliteTreeStore::open(&database).unwrap();
    let _ = store.save(&live).unwrap();
    let mut memory = live.clone();
    memory.clear_feed_views();
    delete_views(&database);
    let mut sql = store.load("tree").unwrap().unwrap().rows().clone();
    assert_eq!(law_rows(&memory), law_rows(&sql), "deleted rows differ");
    for rebuilt in [&mut memory, &mut sql] {
        rebuilt
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        rebuilt
            .project_feed(
                "root",
                &rebuilt.tree_id.clone(),
                &[],
                &TreeFeedReads::default(),
            )
            .unwrap();
    }
    assert_eq!(law_rows(&memory), law_rows(&sql));
    assert_eq!(law_rows(&memory), law_rows(&live));
}

fn delete_views(database: &std::path::Path) {
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute_batch(
            "UPDATE agent_nodes SET fold_order=NULL;
             UPDATE agent_nodes SET status='running',active_turn=0,session_locator=NULL
                 WHERE parent_agent_id IS NOT NULL;
             UPDATE agent_nodes SET attempt_id=(SELECT attempt_id FROM agent_attempts
                 WHERE agent_attempts.tree_id=agent_nodes.tree_id
                 AND agent_attempts.agent_id=agent_nodes.agent_id AND retry_of IS NULL),
                 output_schema_json=(SELECT output_schema_json FROM agent_attempts
                 WHERE agent_attempts.tree_id=agent_nodes.tree_id
                 AND agent_attempts.agent_id=agent_nodes.agent_id AND retry_of IS NULL),
                 schema_digest=(SELECT schema_digest FROM agent_attempts
                 WHERE agent_attempts.tree_id=agent_nodes.tree_id
                 AND agent_attempts.agent_id=agent_nodes.agent_id AND retry_of IS NULL)
                 WHERE parent_agent_id IS NOT NULL;
             UPDATE agent_attempts SET applied_terminal_read_seqs='[]',status='running',output_json=NULL,candidate_id=NULL,turn_id=NULL,ended_at=NULL,blocked_kind=NULL,blocked_account=NULL,blocked_resets_at=NULL,
                 failure_kind=NULL,failure_message=NULL,turns=0,tool_calls=0,
                 input_tokens=0,output_tokens=0,submit_harness_nudge=0;
             UPDATE agent_messages SET durable='accepted' WHERE durable='acknowledged' AND (
                 EXISTS (SELECT 1 FROM agent_commands, json_each(agent_commands.message_ids) AS ids
                     WHERE agent_commands.tree_id=agent_messages.tree_id AND fact='accepted'
                     AND ids.value=agent_messages.message_id)
                 OR EXISTS (SELECT 1 FROM agent_operations,
                     json_each(agent_operations.canonical_request, '$.message_ids') AS ids
                     WHERE agent_operations.tree_id=agent_messages.tree_id
                     AND tool_name IN('failed_wake_mail','undeliverable_mail')
                     AND ids.value=agent_messages.message_id));
             DELETE FROM agent_operations WHERE tool_name='returned_admission'
                 AND EXISTS (SELECT 1 FROM agent_commands
                     WHERE agent_commands.tree_id=agent_operations.tree_id
                     AND agent_commands.command_id=agent_operations.operation_id);
             UPDATE agent_commands SET fact=NULL,turn_id=NULL,accepted_seq=NULL;
             DELETE FROM agent_feed_cursors;",
        )
        .unwrap();
    drop(connection);
}

#[tokio::test]
async fn sqlite_cold_host_anchor_survives_a_refused_feed_read() {
    assert_cold_host_anchor(false).await;
}

#[tokio::test]
async fn sqlite_cold_below_retention_host_anchor_survives_a_refused_feed_read() {
    assert_cold_host_anchor(true).await;
}

#[allow(
    clippy::too_many_lines,
    reason = "cold-read failure, saved host intent, and two SQLite reopens share one fixture"
)]
async fn assert_cold_host_anchor(below_retention: bool) {
    for materialization in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let (mut tree, vcs, spawned) = spawned_tree(tmp.path());
        let child = &spawned.agent_id;
        let accepted = vec![record(
            1,
            FeedFact::CommandAccepted {
                command_id: CommandId::new("start").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(TURN.parse().unwrap()),
                sequence: 1,
                operation_id: None,
            },
        )];
        tree.project_feed(child, SESSION, &accepted, &TreeFeedReads::default())
            .unwrap();
        let later = record(
            2,
            FeedFact::HistoryAppended {
                turn_id: Some(TURN.parse().unwrap()),
                segment: HistorySegmentKind::ToolExchange,
                first_ordinal: 0,
                count: 1,
                first_entry: "00000000-0000-4000-8000-000000004001".parse().unwrap(),
                last_entry: "00000000-0000-4000-8000-000000004001".parse().unwrap(),
                tool_results: Vec::new(),
            },
        );
        tree.project_feed(child, SESSION, &[later], &TreeFeedReads::default())
            .unwrap();
        assert_eq!(tree.feed_cursor(SESSION), 2);
        let head = tree
            .node(child)
            .unwrap()
            .session_locator
            .as_ref()
            .unwrap()
            .parse()
            .unwrap();
        let retained = TreeFeed {
            session_id: SESSION.to_owned(),
            retained_from: 30,
            below_retained: Some(29),
            heads: vec![head],
            head_turns: std::collections::BTreeMap::from([(head, Some(TURN.parse().unwrap()))]),
            records: Vec::new(),
            reads: TreeFeedReads::default(),
        };
        if below_retention {
            tree.clear_feed_views();
            retained.apply(&mut tree, child).unwrap();
            assert_eq!(tree.attempt(child).unwrap().turn_id.as_deref(), Some(TURN));
        }
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        let mut cold = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        // A real feed read fails before any projector pass can warm the caches.
        let sessions = plakous::sessions::AgentSessions::new(
            plakous::sessions::SessionBackend::memory().unwrap(),
        )
        .unwrap();
        let request = plakous::sessions::feed::FeedPageRequest::after(
            plakous::sessions::feed::FeedCursor::new(
                SESSION.parse().unwrap(),
                plakous::sessions::feed::FeedSeq::new(cold.feed_cursor(SESSION)),
            ),
        );
        assert!(sessions.feed_page(request).await.is_err());
        if materialization {
            cold.record_host_materialization(child).unwrap();
        } else {
            cold.record_host_turn_failure(
                child,
                FailedTurnDisposition::Failed {
                    kind: "turn_failed",
                },
                "host failure after unavailable feed".into(),
                Vec::new(),
            )
            .unwrap();
        }
        let intent = cold
            .operations()
            .values()
            .find(|op| {
                op.tool_name
                    == if materialization {
                        "host_decision"
                    } else {
                        "host_turn_failure"
                    }
            })
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&intent.canonical_request).unwrap();
        assert_eq!(
            intent.fold.unwrap().seq,
            if below_retention { 29 } else { 2 }
        );
        assert!(request.get("anchor_seq").is_none());
        assert!(request.get("anchor_below_retained_from").is_none());
        cold.project_feed(child, SESSION, &[], &TreeFeedReads::default())
            .unwrap();
        let effects = cold.take_effects();
        assert_eq!(effects.seals.len(), 1);
        let _ = cold.complete(effects.run(&*vcs));
        // Once the feed becomes readable, both live and rebuilt views see
        // the same retained-head binding evidence.
        retained.apply(&mut cold, child).unwrap();
        assert!(cold.take_effects().is_empty());
        let mut expected = cold.attempt(child).unwrap().clone();
        expected.usage.turns = 0;
        let expected_node = cold.node(child).unwrap().clone();
        let _ = store.save(&cold).unwrap();
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        retained.apply(&mut rebuilt, child).unwrap();
        assert_eq!(rebuilt.attempt(child).unwrap(), &expected);
        assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
        assert!(rebuilt.take_effects().is_empty());
        assert_eq!(
            rebuilt
                .operations()
                .values()
                .filter(|op| { op.tool_name == "consumed_below_retention" })
                .count(),
            0
        );
        let _ = store.save(&rebuilt).unwrap();
        let mut again = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        retained.apply(&mut again, child).unwrap();
        assert_eq!(again.attempt(child).unwrap(), &expected);
        assert!(again.take_effects().is_empty());
    }
}

#[test]
fn sqlite_below_retention_host_outcomes_equal_live_without_new_seals() {
    for decision in [0, 3, 5] {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let (mut tree, vcs, spawned) = spawned_tree(tmp.path());
        let child = &spawned.agent_id;
        let mut feed = vec![record(
            1,
            FeedFact::CommandAccepted {
                command_id: CommandId::new("start").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(TURN.parse().unwrap()),
                sequence: 1,
                operation_id: None,
            },
        )];
        tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        record_decision(&mut tree, child, &mut feed, decision);
        tree.fold_due(child).unwrap();
        let effects = tree.take_effects();
        assert_eq!(effects.seals.len(), 1);
        let _ = tree.complete(effects.run(&*vcs));
        tree.seed_head_turn(child, None).unwrap();
        let mut expected = tree.attempt(child).unwrap().clone();
        expected.usage.turns = 0;
        let mut expected_node = tree.node(child).unwrap().clone();
        expected_node.fold_order = Some(0);
        let head = expected_node
            .session_locator
            .as_ref()
            .unwrap()
            .parse()
            .unwrap();
        let retained = TreeFeed {
            session_id: SESSION.to_owned(),
            retained_from: 30,
            below_retained: Some(29),
            heads: vec![head],
            head_turns: std::collections::BTreeMap::from([(head, None)]),
            records: Vec::new(),
            reads: TreeFeedReads::default(),
        };
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        retained.apply(&mut rebuilt, child).unwrap();
        assert_eq!(
            rebuilt.attempt(child).unwrap(),
            &expected,
            "decision={decision}"
        );
        assert_eq!(
            rebuilt.node(child).unwrap(),
            &expected_node,
            "decision={decision}"
        );
        assert!(rebuilt.take_effects().is_empty());
        let _ = store.save(&rebuilt).unwrap();
        let mut again = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        retained.apply(&mut again, child).unwrap();
        assert_eq!(again.attempt(child).unwrap(), &expected);
        assert!(again.take_effects().is_empty());
    }
}

#[test]
fn sqlite_orphaned_vcs_faults_survive_view_deletion_and_a_second_reopen() {
    for adopt in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("subagents.sqlite3");
        let (mut tree, vcs, spawned) = spawned_tree(tmp.path());
        let child = &spawned.agent_id;
        let feed = vec![record(
            1,
            FeedFact::CommandAccepted {
                command_id: CommandId::new("start").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(TURN.parse().unwrap()),
                sequence: 1,
                operation_id: None,
            },
        )];
        tree.project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        tree.record_host_cancellation(child).unwrap();
        tree.fold_due(child).unwrap();
        if adopt {
            let effects = tree.take_effects();
            let _ = tree.complete(effects.run(&*vcs));
            let candidate = tree.attempt(child).unwrap().candidate_id.clone().unwrap();
            tree.begin_adopt(
                "root",
                &AdoptCandidateRequest {
                    candidate_id: candidate,
                    expected_target: None,
                    evidence: None,
                },
            )
            .unwrap();
        }
        let store = SqliteTreeStore::open(&database).unwrap();
        let _ = store.save(&tree).unwrap();
        let mut reopened = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        assert_eq!(
            reopened.node(child).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        reopened
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        let expected_attempt = reopened.attempt(child).unwrap().clone();
        let expected_node = reopened.node(child).unwrap().clone();
        let _ = store.save(&reopened).unwrap();
        drop(store);
        delete_views(&database);
        let store = SqliteTreeStore::open(&database).unwrap();
        let mut rebuilt = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        rebuilt
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(rebuilt.node(child).unwrap(), &expected_node);
        assert_eq!(rebuilt.attempt(child).unwrap(), &expected_attempt);
        assert!(rebuilt.take_effects().is_empty());
        assert!(rebuilt.fault_reason(child).is_some());
        let _ = store.save(&rebuilt).unwrap();
        let mut again = ProductAgentTree::reopen(store.load("tree").unwrap().unwrap());
        again
            .project_feed(child, SESSION, &feed, &TreeFeedReads::default())
            .unwrap();
        assert_eq!(again.node(child).unwrap(), &expected_node);
        assert!(again.take_effects().is_empty());
    }
}

fn record_decision(
    tree: &mut ProductAgentTree,
    child: &str,
    feed: &mut Vec<FeedRecord>,
    decision: u8,
) {
    match decision {
        0 => tree.record_host_cancellation(child).unwrap(),
        1 => tree.record_host_materialization(child).unwrap(),
        2 | 4 => tree
            .record_host_retirement(child, "release uncertain".into())
            .unwrap(),
        5 => tree
            .record_host_turn_failure(
                child,
                FailedTurnDisposition::Failed {
                    kind: "turn_failed",
                },
                "host failure".into(),
                Vec::new(),
            )
            .unwrap(),
        _ => {
            feed.push(record(
                2,
                FeedFact::TurnTerminal {
                    turn_id: TURN.parse().unwrap(),
                    command_id: CommandId::new("start").unwrap(),
                    outcome: FeedTurnOutcome::Completed {
                        finish: FinishReason::Stop,
                    },
                    closed_exchange: true,
                    steering_settled_unapplied: false,
                    usage: TurnUsage::default(),
                },
            ));
            let projection = tree
                .project_feed(child, SESSION, feed, &TreeFeedReads::default())
                .unwrap();
            tree.record_child_finalization(
                child,
                TURN,
                ChildFinalizationDecision::Finalize,
                true,
                Some(projection.ended_turns.last().unwrap().1.clone()),
            )
            .unwrap();
        }
    }
}

fn spawned_tree(
    root: &std::path::Path,
) -> (
    ProductAgentTree,
    Arc<FakeVcs>,
    plakous_recursive_subagents::tree::SpawnAgentOutput,
) {
    spawned_tree_with_session(root, true)
}

fn spawned_tree_with_session(
    root: &std::path::Path,
    allocated: bool,
) -> (
    ProductAgentTree,
    Arc<FakeVcs>,
    plakous_recursive_subagents::tree::SpawnAgentOutput,
) {
    let vcs = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        "tree",
        "root",
        "owner",
        root.to_owned(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut request = SpawnAgentRequest::test_named("implement", Tier::Mutate, "worker", "task");
    request.workspace = SpawnWorkspace::Fork;
    let spawned = tree.spawn_agent("root", &request, None).unwrap();
    let child = &spawned.agent_id;
    if allocated {
        tree.record_child_session(
        child,
        &format!(
            "{SESSION}/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
        ),
    )
    .unwrap();
        tree.record_command(AgentCommand::intent(
            "start",
            child,
            Some(spawned.attempt_id.clone()),
            AgentCommandKind::Start,
            "task",
            Vec::new(),
        ))
        .unwrap();
    }
    (tree, vcs, spawned)
}
