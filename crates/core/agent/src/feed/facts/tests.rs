//! The fact function over synthetic parent/successor checkpoint pairs.

use std::sync::Arc;

use plakous_model::{
    Actor, CallId, CanonicalHistory, ContentBlock, ConversationItem, Digest, DigestDomain, EntryId,
    HistorySegment, HistorySegmentKind, NamespaceId, Provenance, TextBlock, ToolCallBlock,
    ToolCallState, ToolCallerLineage, ToolCorrelation, ToolExecutionOwner, ToolName, TurnOrigin,
};
use uuid::Uuid;

use super::{commit_facts, genesis_facts};
use crate::checkpoint_bundle::tests::{fixture_with_history_and_ledger, history_segment};
use crate::feed::{
    FeedCallerLineage, FeedCommandKind, FeedCompactReason, FeedFact, FeedOperationKind,
    FeedOperationState, FeedWorkflowKind, FeedWorkflowOutcome,
};
use crate::manifest::RuntimeOperationRoot;
use crate::{
    CheckpointId, CommandId, ControlErrorKind, DurableCommandDisposition, DurableCommandRecord,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, ReconstructiveComponent, RuntimeCheckpoint, RuntimeComponentKind,
    RuntimeLifecycle, RuntimeOperationLedger, SessionWorkflowId,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The fixture names checkpoint `n` as id `20_000 + n`.
const PARENT: u128 = 1;
const SUCCESSOR: u128 = 2;

fn id<T: TryFrom<Uuid>>(value: u128) -> TestResult<T>
where
    T::Error: std::error::Error + 'static,
{
    Ok(T::try_from(Uuid::from_u128(value))?)
}

fn checkpoint_id(number: u128) -> TestResult<CheckpointId> {
    id(20_000 + number)
}

fn pair(
    parent_history: CanonicalHistory,
    successor_history: CanonicalHistory,
    successor_ledger: RuntimeOperationLedger,
) -> TestResult<(RuntimeCheckpoint, Arc<RuntimeCheckpoint>)> {
    let empty = RuntimeOperationLedger::new(Vec::new(), Vec::new())?;
    let parent = fixture_with_history_and_ledger(
        parent_history,
        PARENT,
        false,
        Some(empty),
        Some(RuntimeLifecycle::Quiescent),
    )?;
    let successor = fixture_with_history_and_ledger(
        successor_history,
        SUCCESSOR,
        false,
        Some(successor_ledger),
        Some(RuntimeLifecycle::Quiescent),
    )?;
    Ok((parent, Arc::new(successor)))
}

fn history(segments: &[usize]) -> TestResult<CanonicalHistory> {
    Ok(CanonicalHistory::new(
        segments
            .iter()
            .map(|index| history_segment(*index))
            .collect::<Result<Vec<_>, _>>()?,
    )?)
}

fn component(
    kind: RuntimeComponentKind,
    bytes: impl Into<Vec<u8>>,
) -> TestResult<ReconstructiveComponent> {
    Ok(ReconstructiveComponent::new(kind, 1, bytes.into())?)
}

fn frame(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    bytes.extend_from_slice(value);
}

/// A command record in the runtime's exact envelope encoding.
fn command(
    command_id: &str,
    sequence: u64,
    tag: &[u8],
    accepted: bool,
) -> TestResult<DurableCommandRecord> {
    let mut canonical = Vec::new();
    frame(&mut canonical, tag);
    frame(&mut canonical, b"text");
    let envelope = serde_json::json!({
        "version": 1,
        "canonical_input": canonical,
        "effect": if accepted { "Root" } else { "Rejected" },
        "rejection": (!accepted).then(|| serde_json::json!({"kind": "Stale", "message": "stale"})),
    });
    Ok(DurableCommandRecord::new(
        command_id,
        sequence,
        accepted.then(|| id(7_100)).transpose()?,
        Digest::sha256(DigestDomain::Blob, 1, &[command_id.as_bytes()])?,
        component(
            RuntimeComponentKind::Command,
            serde_json::to_vec(&envelope)?,
        )?,
        if accepted {
            DurableCommandDisposition::Accepted
        } else {
            DurableCommandDisposition::Rejected
        },
        (!accepted).then(|| "Stale".to_owned()),
    )?)
}

/// An offline compaction request in the runtime's exact record encoding.
fn offline_request(workflow_id: SessionWorkflowId, operation_id: &str) -> TestResult<Vec<u8>> {
    let digest = serde_json::to_value(Digest::sha256(DigestDomain::Blob, 1, &[b"digest"])?)?;
    Ok(serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "workflow_id": workflow_id,
        "turn_fence": false,
        "operation_id": operation_id,
        "source_session": id::<plakous_model::SessionId>(1)?,
        "source_branch": id::<plakous_model::BranchId>(2)?,
        "source_checkpoint": checkpoint_id(PARENT)?,
        "source_history": digest,
        "source_projection": digest,
        "source_harness_plan": digest,
        "selected_start": 0,
        "selected_end": 1,
        "replaced_results": 1,
        "replaced_bytes": 10,
        "before_measurement": digest,
        "before_status": digest,
        "before_input_tokens": 900,
        "target_affinity_entropy": ([0_u8; 32]),
    }))?)
}

/// Prepared and dispatched before the parent; completed and finalized by the
/// successor's commit.
fn settled_here(
    operation_id: u64,
    kind: DurableOperationKind,
    request: ReconstructiveComponent,
) -> TestResult<DurableOperationRecord> {
    let evidence = component(RuntimeComponentKind::Operation, b"outcome".to_vec())?;
    Ok(DurableOperationRecord::new(
        operation_id,
        kind,
        request,
        vec![
            DurableOperationTransition::new(
                id(30_000 + u128::from(operation_id))?,
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                id(31_000 + u128::from(operation_id))?,
                DurableOperationState::Dispatched,
            ),
            DurableOperationTransition::new(
                checkpoint_id(SUCCESSOR)?,
                DurableOperationState::Completed(evidence.clone()),
            ),
            DurableOperationTransition::new(
                checkpoint_id(SUCCESSOR)?,
                DurableOperationState::Finalized(evidence),
            ),
        ],
    )?)
}

#[test]
fn a_genesis_names_its_operation_and_whether_it_was_seeded() -> TestResult {
    let empty = fixture_with_history_and_ledger(history(&[])?, 0, false, None, None)?;
    let seeded = fixture_with_history_and_ledger(history(&[0])?, 0, false, None, None)?;
    assert_eq!(
        genesis_facts("create:one", &empty),
        [FeedFact::SessionCreated {
            operation_id: "create:one".to_owned(),
            seeded: false
        }]
    );
    assert!(matches!(
        genesis_facts("create:two", &seeded)[..],
        [FeedFact::SessionCreated { seeded: true, .. }]
    ));
    Ok(())
}

#[test]
fn appended_segments_are_one_fact_each_at_their_ordinals() -> TestResult {
    let parent = history(&[0, 1])?;
    let mut successor = parent.clone();
    for index in [2, 3] {
        successor = successor.append(history_segment(index)?)?;
    }
    let empty = RuntimeOperationLedger::new(Vec::new(), Vec::new())?;
    let (parent, successor) = pair(parent, successor, empty)?;
    let expected = [2_u64, 3]
        .into_iter()
        .map(|index| {
            let entry = id::<EntryId>(10_000 + u128::from(index))?;
            Ok(FeedFact::HistoryAppended {
                turn_id: None,
                segment: HistorySegmentKind::Conversation,
                first_ordinal: index,
                count: 1,
                first_entry: entry,
                last_entry: entry,
                tool_results: Vec::new(),
            })
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(commit_facts(&parent, &successor)?, expected);
    Ok(())
}

#[test]
fn a_replaced_segment_is_appended_but_a_moved_one_is_not() -> TestResult {
    // A withheld prompt or a compaction rewrites history: the new segment
    // after the common prefix is a fact, the parent's retained one is not.
    let replacement = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![ConversationItem::new(
            id(40_000)?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("withheld", None)?)],
            Provenance::Instruction,
        )?],
    )?;
    let parent = history(&[0, 1, 2])?;
    let successor =
        CanonicalHistory::new(vec![history_segment(0)?, replacement, history_segment(2)?])?;
    let empty = RuntimeOperationLedger::new(Vec::new(), Vec::new())?;
    let (parent, successor) = pair(parent, successor, empty)?;
    let facts = commit_facts(&parent, &successor)?;
    assert!(
        matches!(
            &facts[..],
            [FeedFact::HistoryAppended {
                segment: HistorySegmentKind::RecentIntent,
                first_ordinal: 1,
                count: 1,
                ..
            }]
        ),
        "{facts:?}"
    );
    Ok(())
}

#[test]
fn commands_operations_and_workflows_report_only_this_commits_delta() -> TestResult {
    let call = ToolCallBlock::new(
        id::<CallId>(8_001)?,
        ToolName::new("read_file")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        r#"{"path":"src/lib.rs"}"#,
    )?;
    let effect_request =
        serde_json::to_vec(&(Digest::sha256(DigestDomain::Blob, 1, &[b"binding"])?, &call))?;
    let turn_id = id(7_100)?;
    let workflow_id = SessionWorkflowId::new([0x33; 32])?;
    let ledger = RuntimeOperationLedger::new(
        vec![
            command("prompt-1", 1, b"start", true)?,
            command("steer-1", 2, b"steer", false)?,
        ],
        vec![
            settled_here(
                1,
                DurableOperationKind::Provider {
                    root: RuntimeOperationRoot::Turn(turn_id),
                    attempt_id: id(8_000)?,
                },
                component(RuntimeComponentKind::Operation, b"provider".to_vec())?,
            )?,
            settled_here(
                2,
                DurableOperationKind::Effect {
                    turn_id,
                    attempt_id: id(8_000)?,
                    call_id: call.call_id(),
                    source_index: 0,
                },
                component(RuntimeComponentKind::Operation, effect_request)?,
            )?,
            DurableOperationRecord::new(
                3,
                DurableOperationKind::Compaction { workflow_id },
                component(
                    RuntimeComponentKind::Operation,
                    offline_request(workflow_id, "compact:one")?,
                )?,
                vec![DurableOperationTransition::new(
                    checkpoint_id(SUCCESSOR)?,
                    DurableOperationState::Finalized(component(
                        RuntimeComponentKind::Operation,
                        b"nothing".to_vec(),
                    )?),
                )],
            )?,
        ],
    )?;
    let (parent, successor) = pair(history(&[0])?, history(&[0])?, ledger)?;
    let facts = commit_facts(&parent, &successor)?;
    let compact = FeedWorkflowKind::Compact {
        reason: FeedCompactReason::Endpoint,
    };
    let advanced = |operation_id, kind: &FeedOperationKind, state| FeedFact::OperationAdvanced {
        operation_id,
        turn_id: (operation_id != 3).then_some(turn_id),
        kind: kind.clone(),
        state,
    };
    let provider = FeedOperationKind::ProviderAttempt {
        attempt_id: id(8_000)?,
    };
    let effect = FeedOperationKind::Effect {
        call_id: call.call_id(),
        exposed_name: ToolName::new("read_file")?,
        lineage: FeedCallerLineage::Direct,
        subject: Some("src/lib.rs".to_owned()),
    };
    let compaction = FeedOperationKind::Compaction { workflow_id };
    assert_eq!(
        facts,
        [
            FeedFact::CommandAccepted {
                command_id: CommandId::new("prompt-1")?,
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt
                },
                turn_id: Some(turn_id),
                sequence: 1,
                operation_id: None,
            },
            FeedFact::CommandRejected {
                command_id: CommandId::new("steer-1")?,
                kind: FeedCommandKind::Steer,
                error: ControlErrorKind::Stale,
            },
            FeedFact::WorkflowAdmitted {
                workflow_id,
                operation_id: Some("compact:one".to_owned()),
                kind: compact,
            },
            advanced(1, &provider, FeedOperationState::Completed),
            advanced(1, &provider, FeedOperationState::Finalized),
            advanced(2, &effect, FeedOperationState::Completed),
            advanced(2, &effect, FeedOperationState::Finalized),
            advanced(3, &compaction, FeedOperationState::Finalized),
            FeedFact::WorkflowSettled {
                workflow_id,
                operation_id: Some("compact:one".to_owned()),
                kind: compact,
                outcome: FeedWorkflowOutcome::NoEligibleRange,
            },
        ]
    );
    Ok(())
}

#[test]
fn an_unchanged_successor_has_no_facts() -> TestResult {
    let empty = RuntimeOperationLedger::new(Vec::new(), Vec::new())?;
    let (parent, successor) = pair(history(&[0, 1])?, history(&[0, 1])?, empty)?;
    assert_eq!(commit_facts(&parent, &successor)?, []);
    Ok(())
}

fn text_segment(entry: u128, text: &str) -> TestResult<HistorySegment> {
    Ok(HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![ConversationItem::new(
            id(entry)?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(text, None)?)],
            Provenance::UserInput,
        )?],
    )?)
}

#[test]
fn an_in_place_rewrite_of_thousands_of_segments_is_one_bounded_fact() -> TestResult {
    const SEGMENTS: u128 = 5_000;
    let parent = CanonicalHistory::new(
        (0..SEGMENTS)
            .map(|index| text_segment(50_000 + index, &format!("tool output {index}")))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    let mut rewritten = (0..SEGMENTS)
        .map(|index| text_segment(50_000 + index, "[output replaced]"))
        .collect::<Result<Vec<_>, _>>()?;
    rewritten.push(text_segment(90_000, "notice")?);
    let successor = CanonicalHistory::new(rewritten)?;
    let empty = RuntimeOperationLedger::new(Vec::new(), Vec::new())?;
    let (parent, successor) = pair(parent, successor, empty)?;
    let facts = commit_facts(&parent, &successor)?;
    assert!(
        matches!(
            &facts[..],
            [
                FeedFact::HistoryAppended {
                    first_ordinal: 5_000,
                    count: 1,
                    ..
                },
                FeedFact::HistoryRewritten {
                    first_ordinal: 0,
                    count: 5_000,
                    ..
                }
            ]
        ),
        "{:?}",
        &facts[..facts.len().min(3)]
    );
    let record = crate::feed::FeedRecord::from_store(
        crate::feed::FeedHeader {
            session_id: id(1)?,
            seq: crate::feed::FeedSeq::new(1),
            branch_id: id(2)?,
            committed_at_ms: 0,
            change: crate::feed::FeedChange::Archived {
                revision: crate::session::SessionRevision::from_store(1),
            },
        },
        facts,
    );
    assert!(record.encode()?.len() < 1_024);
    Ok(())
}

#[test]
fn a_provider_programmatic_call_names_its_hosted_program() -> TestResult {
    let call = ToolCallBlock::new(
        id::<CallId>(8_101)?,
        ToolName::new("read_file")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-1".to_owned()),
            None,
            ToolCallerLineage::provider_programmatic(
                NamespaceId::new("code_execution")?,
                "exec-7",
            )?,
        )?,
        r#"{"path":"src/main.rs"}"#,
    )?;
    let request =
        serde_json::to_vec(&(Digest::sha256(DigestDomain::Blob, 1, &[b"binding"])?, &call))?;
    let turn_id = id(7_100)?;
    let ledger = RuntimeOperationLedger::new(
        Vec::new(),
        vec![settled_here(
            4,
            DurableOperationKind::Effect {
                turn_id,
                attempt_id: id(8_000)?,
                call_id: call.call_id(),
                source_index: 0,
            },
            component(RuntimeComponentKind::Operation, request)?,
        )?],
    )?;
    let (parent, successor) = pair(history(&[0])?, history(&[0])?, ledger)?;
    let facts = commit_facts(&parent, &successor)?;
    assert!(
        facts.iter().any(|fact| matches!(
            fact,
            FeedFact::OperationAdvanced {
                kind: FeedOperationKind::Effect {
                    lineage: FeedCallerLineage::ProviderProgrammatic { kind, execution_id },
                    ..
                },
                ..
            } if kind.as_str() == "code_execution" && execution_id == "exec-7"
        )),
        "{facts:?}"
    );
    Ok(())
}
