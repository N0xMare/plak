//! Feed record codec tests.

use plakous_model::{
    Digest, DigestDomain, FinishReason, HistorySegmentKind, ModelErrorKind, NamespaceId, TurnOrigin,
};
use uuid::Uuid;

use super::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn id<T: TryFrom<Uuid>>(value: u128) -> TestResult<T>
where
    T::Error: std::error::Error + 'static,
{
    Ok(T::try_from(Uuid::from_u128(value))?)
}

fn locator(checkpoint: u128) -> TestResult<FeedLocator> {
    Ok(FeedLocator {
        session_id: id(1)?,
        branch_id: id(2)?,
        checkpoint_id: id(checkpoint)?,
    })
}

/// One record of every change and one fact of every kind and summary shape.
fn every_shape() -> TestResult<Vec<FeedRecord>> {
    let workflow_id = SessionWorkflowId::new([7; 32])?;
    let compact = FeedWorkflowKind::Compact {
        reason: FeedCompactReason::Endpoint,
    };
    let facts = vec![
        FeedFact::SessionCreated {
            operation_id: "create:one".to_owned(),
            seeded: true,
        },
        FeedFact::CommandAccepted {
            command_id: CommandId::new("cmd-1")?,
            kind: FeedCommandKind::Root {
                origin: TurnOrigin::HostWake,
            },
            turn_id: Some(id(10)?),
            sequence: 3,
            operation_id: None,
        },
        FeedFact::CommandAccepted {
            command_id: CommandId::new("cmd-5")?,
            kind: FeedCommandKind::Steer,
            turn_id: Some(id(10)?),
            sequence: 4,
            operation_id: Some(12),
        },
        FeedFact::CommandRejected {
            command_id: CommandId::new("cmd-2")?,
            kind: FeedCommandKind::Steer,
            error: ControlErrorKind::Stale,
        },
        FeedFact::HistoryAppended {
            turn_id: None,
            segment: HistorySegmentKind::ToolExchange,
            first_ordinal: 4,
            count: 2,
            first_entry: id(11)?,
            last_entry: id(12)?,
            tool_results: vec![id(13)?],
        },
        FeedFact::HistoryRewritten {
            turn_id: Some(id(10)?),
            first_ordinal: 0,
            count: 4,
        },
        FeedFact::OperationAdvanced {
            operation_id: 9,
            turn_id: Some(id(10)?),
            kind: FeedOperationKind::Effect {
                call_id: id(13)?,
                exposed_name: ToolName::new("read")?,
                lineage: FeedCallerLineage::LocalProgrammatic {
                    evaluation_operation_id: 8,
                    parent_call_id: Some(id(16)?),
                    cell_id: "cell-1".to_owned(),
                    request_sequence: 1,
                    depth: 1,
                },
                subject: Some("src/lib.rs".to_owned()),
            },
            state: FeedOperationState::Dispatched,
        },
        FeedFact::OperationAdvanced {
            operation_id: 11,
            turn_id: Some(id(10)?),
            kind: FeedOperationKind::Effect {
                call_id: id(17)?,
                exposed_name: ToolName::new("read")?,
                lineage: FeedCallerLineage::ProviderProgrammatic {
                    kind: NamespaceId::new("code_execution")?,
                    execution_id: "exec-1".to_owned(),
                },
                subject: None,
            },
            state: FeedOperationState::Completed,
        },
        FeedFact::OperationAdvanced {
            operation_id: 8,
            turn_id: Some(id(10)?),
            kind: FeedOperationKind::CodeCell {
                call_id: id(16)?,
                exposed_name: ToolName::new("exec")?,
                subject: Some("print(1)".to_owned()),
            },
            state: FeedOperationState::Finalized,
        },
        FeedFact::OperationAdvanced {
            operation_id: 10,
            turn_id: None,
            kind: FeedOperationKind::ProcessSession {
                job_number: Some(2),
            },
            state: FeedOperationState::OutcomeUnknown,
        },
        FeedFact::OperationAdvanced {
            operation_id: 7,
            turn_id: None,
            kind: FeedOperationKind::Compaction { workflow_id },
            state: FeedOperationState::ProvenNotDispatched,
        },
        FeedFact::OperationAdvanced {
            operation_id: 6,
            turn_id: Some(id(10)?),
            kind: FeedOperationKind::ProviderAttempt {
                attempt_id: id(18)?,
            },
            state: FeedOperationState::Dispatched,
        },
        FeedFact::TurnTerminal {
            turn_id: id(10)?,
            command_id: CommandId::new("cmd-1")?,
            outcome: FeedTurnOutcome::Failed {
                kind: AgentErrorKind::Model,
                model_kind: Some(ModelErrorKind::ContextLimit),
                context: Some(FeedContextLimit {
                    withheld: true,
                    remedy: ContextOverflowRemedy::ResendShorter,
                }),
            },
            closed_exchange: true,
            steering_settled_unapplied: false,
            usage: TurnUsage::default(),
        },
        FeedFact::TurnTerminal {
            turn_id: id(14)?,
            command_id: CommandId::new("cmd-3")?,
            outcome: FeedTurnOutcome::Cancelled {
                cause: FeedCancelCause::Abandoned {
                    claim: AbandonedTurnClaim::ModelRequest,
                    cause: AbandonmentCause::ProcessLost,
                    model_round_completed: true,
                    tool_round_completed: false,
                },
            },
            closed_exchange: false,
            steering_settled_unapplied: true,
            usage: TurnUsage::default(),
        },
        FeedFact::TurnTerminal {
            turn_id: id(19)?,
            command_id: CommandId::new("cmd-6")?,
            outcome: FeedTurnOutcome::Cancelled {
                cause: FeedCancelCause::Requested,
            },
            closed_exchange: false,
            steering_settled_unapplied: false,
            usage: TurnUsage::default(),
        },
        FeedFact::TurnTerminal {
            turn_id: id(15)?,
            command_id: CommandId::new("cmd-4")?,
            outcome: FeedTurnOutcome::Completed {
                finish: FinishReason::Stop,
            },
            closed_exchange: false,
            steering_settled_unapplied: false,
            usage: TurnUsage::default(),
        },
        FeedFact::TurnTerminal {
            turn_id: id(23)?,
            command_id: CommandId::new("cmd-7")?,
            outcome: FeedTurnOutcome::Refused,
            closed_exchange: false,
            steering_settled_unapplied: false,
            usage: TurnUsage::default(),
        },
        FeedFact::TurnTerminal {
            turn_id: id(24)?,
            command_id: CommandId::new("cmd-8")?,
            outcome: FeedTurnOutcome::RecoveryRequired,
            closed_exchange: false,
            steering_settled_unapplied: false,
            usage: TurnUsage::default(),
        },
        FeedFact::WorkflowAdmitted {
            workflow_id,
            operation_id: Some("compact:one".to_owned()),
            kind: compact,
        },
        FeedFact::WorkflowSettled {
            workflow_id,
            operation_id: Some("compact:one".to_owned()),
            kind: compact,
            outcome: FeedWorkflowOutcome::Compacted {
                source: locator(20)?,
                target: locator(21)?,
                pass: FeedCompactPass::Summary,
                selected_segments: 12,
                tokens_before: 199_700,
                tokens_after: 12_400,
            },
        },
        FeedFact::WorkflowSettled {
            workflow_id,
            operation_id: None,
            kind: FeedWorkflowKind::Compact {
                reason: FeedCompactReason::TurnFence,
            },
            outcome: FeedWorkflowOutcome::Failed {
                summary_sent: true,
                kind: AgentErrorKind::Model,
                model_kind: Some(ModelErrorKind::Timeout),
            },
        },
        FeedFact::WorkflowSettled {
            workflow_id,
            operation_id: Some("compact:two".to_owned()),
            kind: compact,
            outcome: FeedWorkflowOutcome::Abandoned {
                summary_unsent: true,
            },
        },
        FeedFact::WorkflowSettled {
            workflow_id,
            operation_id: Some("compact:three".to_owned()),
            kind: compact,
            outcome: FeedWorkflowOutcome::NoEligibleRange,
        },
        FeedFact::WorkflowSettled {
            workflow_id,
            operation_id: Some("fork:one".to_owned()),
            kind: FeedWorkflowKind::Fork,
            outcome: FeedWorkflowOutcome::Published {
                target: locator(22)?,
            },
        },
        FeedFact::WorkflowSettled {
            workflow_id,
            operation_id: Some("rebind:one".to_owned()),
            kind: FeedWorkflowKind::Rebind,
            outcome: FeedWorkflowOutcome::Published {
                target: locator(22)?,
            },
        },
    ];
    let changes = [
        FeedChange::Created {
            genesis: id(20)?,
            revision: BranchRevision::INITIAL,
        },
        FeedChange::Advanced {
            parent: id(20)?,
            checkpoint: id(21)?,
            revision: BranchRevision::from_store(1),
            head: FeedHeadKind::Working,
        },
        FeedChange::Branched {
            source: locator(21)?,
            target: id(22)?,
            revision: SessionRevision::from_store(1),
            transition: FeedBranchTransition::Rebind,
        },
        FeedChange::LeaseReleased {
            head: id(21)?,
            fence: 2,
        },
        FeedChange::Pruned {
            head: id(21)?,
            horizon: BranchRevision::from_store(0),
        },
        FeedChange::Archived {
            revision: SessionRevision::from_store(2),
        },
    ];
    changes
        .into_iter()
        .zip(1..)
        .map(|(change, seq)| {
            Ok(FeedRecord::from_store(
                FeedHeader {
                    session_id: id(1)?,
                    seq: FeedSeq::new(seq),
                    branch_id: id(2)?,
                    committed_at_ms: 1_700_000_000_000,
                    change,
                },
                if seq == 2 { facts.clone() } else { Vec::new() },
            ))
        })
        .collect()
}

#[test]
fn every_record_shape_round_trips_canonically() -> TestResult {
    for record in every_shape()? {
        let bytes = record.encode()?;
        assert_eq!(FeedRecord::decode(&bytes)?, record);
    }
    Ok(())
}

#[test]
fn decoding_refuses_another_version_and_noncanonical_bytes() -> TestResult {
    let record = every_shape()?.remove(0);
    let bytes = String::from_utf8(record.encode()?)?;
    let other = bytes.replacen("\"version\":1", "\"version\":2", 1);
    assert_eq!(
        FeedRecord::decode(other.as_bytes()),
        Err(FeedCodecError::UnsupportedVersion(2))
    );
    let spaced = bytes.replacen(':', ": ", 1);
    assert_eq!(
        FeedRecord::decode(spaced.as_bytes()),
        Err(FeedCodecError::Noncanonical)
    );
    assert_eq!(FeedRecord::decode(b"{}"), Err(FeedCodecError::Malformed));
    Ok(())
}

#[test]
fn a_record_above_the_page_bound_does_not_encode() -> TestResult {
    let mut record = every_shape()?.remove(0);
    record.facts = vec![FeedFact::SessionCreated {
        operation_id: "x".repeat(MAX_FEED_RECORD_BYTES),
        seeded: false,
    }];
    assert!(matches!(
        record.encode(),
        Err(FeedCodecError::RecordTooLarge { bytes }) if bytes > MAX_FEED_RECORD_BYTES
    ));
    Ok(())
}

/// The frozen bytes of every record shape. A change here is a codec version
/// bump (or a runtime epoch bump), never a silent re-pin.
#[test]
fn every_record_shape_has_frozen_bytes() -> TestResult {
    const GOLDEN: [&str; 6] = [
        "3c36ee923adfebb5b14c4119f3cdfcad4417f12fe71cc56d2424a8b0f40a6404",
        "7f4570c826c2e60dcfb0a36ca594ba4f2e7d7adf8d3cc6baad6db17f00529ceb",
        "f31813a9b14a515293a7815dd1d02d319526a14809c4b3ed7478a54667e01730",
        "12e78ad89b4f9821070faa2fcaf4e5cf3d6de0b50e745da7b47237e048757d61",
        "2d1111879744c2decedef4fb1ce996a51260d0fa438a4b17a7e6cef266f32d7a",
        "2a9c70a48286f0820c0382f469cff7fed65677f2a56da8368cedeb6cd0e65bd4",
    ];
    let actual = every_shape()?
        .iter()
        .map(|record| {
            let digest = Digest::sha256(DigestDomain::Blob, 1, &[&record.encode()?])?;
            Ok(digest.bytes().iter().fold(String::new(), |mut text, byte| {
                let _ = std::fmt::Write::write_fmt(&mut text, format_args!("{byte:02x}"));
                text
            }))
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(actual, GOLDEN, "feed record golden bytes changed");
    Ok(())
}

#[test]
fn the_workflow_identity_is_lowercase_hex() -> TestResult {
    let record = &every_shape()?[1];
    let text = String::from_utf8(record.encode()?)?;
    assert!(
        text.contains(&format!("\"workflow_id\":\"{}\"", "07".repeat(32))),
        "{text}"
    );
    let upper = text.replacen(&"07".repeat(32), &"0A".repeat(32), 1);
    assert_eq!(
        FeedRecord::decode(upper.as_bytes()),
        Err(FeedCodecError::Malformed)
    );
    Ok(())
}

#[test]
fn decoding_refuses_fields_no_writer_produces() -> TestResult {
    let record = &every_shape()?[1];
    let text = String::from_utf8(record.encode()?)?;
    for (from, to) in [
        ("\"seq\":2", "\"seq\":0"),
        ("\"src/lib.rs\"", "\"src/\\nlib.rs\""),
        ("\"create:one\"", "\"create one\""),
    ] {
        let forged = text.replacen(from, to, 1);
        assert_ne!(forged, text, "{from}");
        assert_eq!(
            FeedRecord::decode(forged.as_bytes()),
            Err(FeedCodecError::Malformed),
            "{to}"
        );
    }
    let long = text.replacen("\"src/lib.rs\"", &format!("\"{}\"", "s".repeat(241)), 1);
    assert_eq!(
        FeedRecord::decode(long.as_bytes()),
        Err(FeedCodecError::Malformed)
    );
    Ok(())
}
