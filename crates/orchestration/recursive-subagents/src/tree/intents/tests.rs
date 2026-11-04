use super::{
    ContinuationAnchor, ContinuationCause, ContinuationDecision, ContinuationIntent, EpochPending,
    TerminalRefusalIntent, VcsExpectedOutcome, VcsStageClass, VcsStageIntent, VcsStageKind,
};
use crate::protocol::{AttemptId, CandidateId};
use crate::tree::test_support::tree;
use crate::tree::{AttemptFailure, FoldAnchor, MemoryTreeStore, OperationRecord, TreeStore};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const TIP: &str = "89abcdef0123456789abcdef0123456789abcdef";
const DIGEST: &str = "aaaabbbbccccddddeeeeffff00001111aaaabbbbccccddddeeeeffff00001111";

fn attempt() -> AttemptId {
    AttemptId::from_persisted("c1-a1".to_owned())
}

/// Saves `operations` on a fresh tree through the in-memory store and hands
/// back the rows a reopen reads.
fn round_trip(operations: Vec<OperationRecord>) -> Vec<OperationRecord> {
    let (_root, mut tree) = tree();
    let ids = operations
        .iter()
        .map(|operation| operation.operation_id.clone())
        .collect::<Vec<_>>();
    for operation in operations {
        tree.operations
            .insert(operation.operation_id.clone(), operation);
    }
    let store = MemoryTreeStore::new();
    let _ = store.save_in_custody(&tree).unwrap();
    // The rows as read, before the reopen law settles any stage they hold.
    let loaded = store.load(&tree.tree_id).unwrap().unwrap().0;
    ids.iter()
        .map(|id| loaded.operations()[id].clone())
        .collect()
}

fn root_continuation(turn: &str, generation: u64, round: u32) -> ContinuationIntent {
    ContinuationIntent {
        attempt_id: None,
        anchor: ContinuationAnchor::Turn {
            turn_id: turn.to_owned(),
        },
        host_generation: generation,
        cause: ContinuationCause::OwnerLost,
        round,
        decided_at_ms: 42,
        decision: ContinuationDecision::NewTurn {
            command_id: format!("plak:continue:{turn}:{generation}"),
            withdrawn: Vec::new(),
        },
    }
}

#[test]
fn continuation_decisions_round_trip_for_the_root_and_a_child() {
    let child = ContinuationIntent {
        attempt_id: Some(attempt()),
        anchor: ContinuationAnchor::Command {
            command_id: "plak:start:c1:1".to_owned(),
        },
        host_generation: 3,
        cause: ContinuationCause::OwnerLost,
        round: 1,
        decided_at_ms: 7,
        decision: ContinuationDecision::Resubmit {
            command_id: "plak:start:c1:1".to_owned(),
        },
    };
    let root = root_continuation("turn-1", 2, 70_000);
    let withdrawn = ContinuationIntent {
        decision: ContinuationDecision::Withdrawn {
            command_ids: vec!["cmd-a".to_owned(), "cmd-b".to_owned()],
        },
        ..root_continuation("turn-2", 2, 1)
    };
    let child_row = child
        .to_operation("c1", Some(FoldAnchor { seq: 3, order: 9 }))
        .unwrap();
    assert_eq!(
        child_row.operation_id,
        "plak:continuation:c1:c1-a1:owner_lost:3:1:command:plak:start:c1:1"
    );
    let root_row = root.to_operation("root", None).unwrap();
    assert_eq!(
        root_row.operation_id,
        "plak:continuation:root:root:owner_lost:2:70000:turn:turn-1"
    );
    let loaded = round_trip(vec![
        child_row.clone(),
        root_row,
        withdrawn.to_operation("root", None).unwrap(),
    ]);
    assert_eq!(loaded[0], child_row);
    assert_eq!(
        ContinuationIntent::from_operation(&loaded[0]).unwrap(),
        Some(child)
    );
    assert_eq!(
        ContinuationIntent::from_operation(&loaded[1]).unwrap(),
        Some(root.clone())
    );
    assert_eq!(
        ContinuationIntent::from_operation(&loaded[2]).unwrap(),
        Some(withdrawn)
    );
    assert_eq!(
        TerminalRefusalIntent::from_operation(&loaded[1]).unwrap(),
        None,
        "another kind is not this one"
    );

    let mut moved = loaded[1].clone();
    moved.caller_agent_id = "c2".to_owned();
    assert!(
        ContinuationIntent::from_operation(&moved).is_err(),
        "a row is valid only under its own identity"
    );
    let mut edited = loaded[1].clone();
    edited.canonical_request = edited.canonical_request.replace("owner_lost", "handoff");
    assert!(
        ContinuationIntent::from_operation(&edited).is_err(),
        "a payload that no longer matches its digest"
    );
    let mut extended = loaded[1].clone();
    extended.canonical_request =
        extended
            .canonical_request
            .replacen('{', r#"{"later_field":1,"#, 1);
    extended.request_digest = crate::tree::digest_text(&extended.canonical_request);
    assert!(
        ContinuationIntent::from_operation(&extended).is_err(),
        "the payload is frozen: an unknown field is refused"
    );
}

/// Root decisions in different turns, and decisions after another host
/// loss, have distinct identities.
#[test]
fn continuation_identities_never_collide_across_turns_and_generations() {
    let ids = [
        root_continuation("turn-1", 1, 1),
        root_continuation("turn-2", 1, 1),
        root_continuation("turn-1", 2, 1),
        root_continuation("turn-1", 1, 2),
    ]
    .map(|intent| intent.operation_id("root"));
    let mut distinct = ids.to_vec();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), ids.len(), "{ids:?}");
}

#[test]
fn incoherent_continuations_are_refused() {
    let base = root_continuation("turn-1", 1, 1);
    let refused = [
        ContinuationIntent {
            round: 0,
            ..base.clone()
        },
        ContinuationIntent {
            host_generation: 0,
            ..base.clone()
        },
        ContinuationIntent {
            anchor: ContinuationAnchor::Turn {
                turn_id: String::new(),
            },
            ..base.clone()
        },
        ContinuationIntent {
            decision: ContinuationDecision::Withdrawn {
                command_ids: Vec::new(),
            },
            ..base.clone()
        },
        ContinuationIntent {
            decision: ContinuationDecision::NewTurn {
                command_id: "a".to_owned(),
                withdrawn: vec!["a".to_owned()],
            },
            ..base.clone()
        },
        ContinuationIntent {
            decision: ContinuationDecision::Withdrawn {
                command_ids: vec!["a".to_owned(), "a".to_owned()],
            },
            ..base
        },
    ];
    for intent in refused {
        assert!(intent.to_operation("root", None).is_err(), "{intent:?}");
    }
}

#[test]
fn terminal_refusals_round_trip() {
    let refusal = TerminalRefusalIntent {
        attempt_id: attempt(),
        failure: AttemptFailure {
            kind: "session_incompatible".to_owned(),
            message: "created by an incompatible build".to_owned(),
        },
        observed_at_ms: 5,
    };
    let row = refusal.to_operation("c1", None).unwrap();
    assert_eq!(row.operation_id, "plak:terminal_refusal:c1:c1-a1");
    let loaded = round_trip(vec![row]);
    assert_eq!(
        TerminalRefusalIntent::from_operation(&loaded[0]).unwrap(),
        Some(refusal.clone())
    );
    let mut blank = refusal;
    blank.failure.kind = String::new();
    assert!(blank.to_operation("c1", None).is_err());
}

fn stage(stage: VcsStageKind, class: VcsStageClass) -> VcsStageIntent {
    VcsStageIntent {
        attempt_id: Some(attempt()),
        stage,
        class,
        agent_id: "c1".to_owned(),
        candidate_id: None,
        source_runner: None,
        target_runner: None,
        base_commit: None,
        tip_commit: None,
        bundle_digest: None,
        expected: None,
    }
}

/// One coherent intent per stage kind, carrying exactly its inputs.
fn coherent_stages() -> Vec<VcsStageIntent> {
    use VcsStageClass::{Ambiguous, Reproducible, Verifiable};
    use VcsStageKind::{
        Adopt, BaseSnapshot, BundleApply, BundleCreate, Mint, Refresh, Relay, Seal, Unmint,
    };
    let candidate = Some(CandidateId::from_persisted("c1-a1-cand".to_owned()));
    let commit = Some(COMMIT.to_owned());
    let tip = Some(TIP.to_owned());
    let source = Some("parent-runner".to_owned());
    let target = Some("child-runner".to_owned());
    vec![
        VcsStageIntent {
            base_commit: commit.clone(),
            ..stage(Mint, Ambiguous)
        },
        VcsStageIntent {
            candidate_id: candidate.clone(),
            base_commit: commit.clone(),
            ..stage(Seal, Reproducible)
        },
        VcsStageIntent {
            candidate_id: candidate.clone(),
            ..stage(Adopt, Ambiguous)
        },
        stage(Unmint, Reproducible),
        VcsStageIntent {
            base_commit: commit.clone(),
            target_runner: target.clone(),
            ..stage(Refresh, Ambiguous)
        },
        VcsStageIntent {
            source_runner: source.clone(),
            base_commit: commit.clone(),
            ..stage(BaseSnapshot, Reproducible)
        },
        VcsStageIntent {
            source_runner: source.clone(),
            target_runner: target.clone(),
            base_commit: commit,
            tip_commit: tip.clone(),
            ..stage(BundleCreate, Reproducible)
        },
        VcsStageIntent {
            source_runner: source.clone(),
            target_runner: target.clone(),
            tip_commit: tip.clone(),
            bundle_digest: Some(DIGEST.to_owned()),
            ..stage(Relay, Reproducible)
        },
        VcsStageIntent {
            candidate_id: candidate,
            source_runner: target,
            target_runner: source,
            tip_commit: tip,
            bundle_digest: Some(DIGEST.to_owned()),
            expected: Some(VcsExpectedOutcome {
                reference: "refs/plak/candidates/c1-a1".to_owned(),
                commit: TIP.to_owned(),
            }),
            ..stage(BundleApply, Verifiable)
        },
    ]
}

#[test]
fn every_vcs_stage_round_trips_with_exactly_its_inputs() {
    let stages = coherent_stages();
    let rows = stages
        .iter()
        .enumerate()
        .map(|(index, intent)| {
            intent
                .to_operation(format!("stage-{index}"), "root", None)
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(rows[7].canonical_request.contains(r#""stage":"relay""#));
    let loaded = round_trip(rows);
    for (row, expected) in loaded.iter().zip(&stages) {
        assert_eq!(
            VcsStageIntent::from_operation(row).unwrap().as_ref(),
            Some(expected)
        );
    }
}

#[test]
fn a_vcs_stage_with_a_missing_extra_or_malformed_input_is_refused() {
    let stages = coherent_stages();
    let refused = [
        // Mint takes no tip; Seal needs its candidate and base.
        VcsStageIntent {
            tip_commit: Some(TIP.to_owned()),
            ..stages[0].clone()
        },
        VcsStageIntent {
            candidate_id: None,
            ..stages[1].clone()
        },
        VcsStageIntent {
            base_commit: None,
            ..stages[1].clone()
        },
        // Local stages name no source runner.
        VcsStageIntent {
            source_runner: Some("elsewhere".to_owned()),
            ..stages[3].clone()
        },
        // A bundle stage is reproducible and needs both runners, distinct.
        VcsStageIntent {
            class: VcsStageClass::Ambiguous,
            ..stages[6].clone()
        },
        VcsStageIntent {
            target_runner: None,
            ..stages[6].clone()
        },
        VcsStageIntent {
            target_runner: stages[6].source_runner.clone(),
            ..stages[6].clone()
        },
        // Create has no digest yet; relay and apply need one, in hex.
        VcsStageIntent {
            bundle_digest: Some(DIGEST.to_owned()),
            ..stages[6].clone()
        },
        VcsStageIntent {
            bundle_digest: None,
            ..stages[7].clone()
        },
        VcsStageIntent {
            bundle_digest: Some("not-hex".to_owned()),
            ..stages[7].clone()
        },
        // Commits are git object ids.
        VcsStageIntent {
            tip_commit: Some("HEAD".to_owned()),
            ..stages[7].clone()
        },
        // Verifiable exactly with an expected outcome, under refs/.
        VcsStageIntent {
            expected: None,
            ..stages[8].clone()
        },
        VcsStageIntent {
            expected: Some(VcsExpectedOutcome {
                reference: "main".to_owned(),
                commit: TIP.to_owned(),
            }),
            ..stages[8].clone()
        },
        VcsStageIntent {
            class: VcsStageClass::Verifiable,
            ..stages[3].clone()
        },
        VcsStageIntent {
            agent_id: "not a node".to_owned(),
            ..stages[3].clone()
        },
        VcsStageIntent {
            source_runner: Some("has space".to_owned()),
            ..stages[5].clone()
        },
    ];
    for intent in refused {
        assert!(
            intent.to_operation("stage", "root", None).is_err(),
            "{intent:?}"
        );
    }
}

#[test]
fn a_pending_epoch_keeps_its_exact_snapshot_bytes() {
    let (_root, tree) = tree();
    let mut successor = tree.snapshot.clone();
    successor.transient_resumes = 2;
    successor.digest = successor.compute_digest();
    let pending = EpochPending::of(1, "switch-workflow-1", &successor).unwrap();
    let row = pending.to_operation("epoch-1", "root", None).unwrap();
    let loaded = round_trip(vec![row]);
    let decoded = EpochPending::from_operation(&loaded[0]).unwrap().unwrap();
    assert_eq!(decoded, pending);
    assert_eq!(decoded.snapshot().unwrap(), successor);

    let mut stale = successor.clone();
    stale.transient_resumes = 0;
    assert!(
        EpochPending::of(1, "w", &stale).is_err(),
        "a digest that does not match"
    );
    assert!(
        EpochPending::of(0, "w", &successor).is_err(),
        "epoch 0 is the header"
    );
    assert!(EpochPending::of(1, "", &successor).is_err());
    let mut forged = pending;
    forged.digest = "0".repeat(64);
    assert!(forged.to_operation("epoch-2", "root", None).is_err());
}
