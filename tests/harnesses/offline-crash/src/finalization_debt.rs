use std::error::Error;
use std::num::NonZeroU64;
use std::path::Path;

use plakous::model::{AttemptId, CallId, CheckpointId, Digest, DigestDomain, RunId, TurnId};
use plakous::sessions::SessionLocator;
use plakous_agent::internal::{
    AcquireBranch, CommitCheckpoint, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, DurableOperationTransition, ReconstructiveComponent, RecoveryAction,
    RecoveryHandlerSignature, RecoveryObligation, RecoveryObligationKind, RuntimeCheckpoint,
    RuntimeCheckpointCodec, RuntimeComponentKind, RuntimeLifecycle, RuntimeManifest,
    RuntimeOperationRoot, RuntimeTopology, RuntimeWorkingRoot, SessionRef, SessionStore,
    WorkingRecoveryPhase,
};
use plakous_store::SqliteSessionStore;
use uuid::Uuid;

/// Seeds exact locally settled sibling debt behind an already-finalized root.
///
/// This repository-only fixture writes a real checkpoint through the store's
/// commit authority; hosted tests need no access to internal checkpoint types.
///
/// # Errors
/// Returns a fixture error if the source does not have a finalized root
/// or if constructing or committing the debt checkpoint fails.
pub async fn seed_finalization_debt(
    database: &Path,
    source: SessionLocator,
    turn_id: TurnId,
) -> Result<SessionLocator, Box<dyn Error + Send + Sync>> {
    seed_terminal_debt(database, source, turn_id, false).await
}

/// Seeds uncertain provider and effect siblings under a recorded root.
///
/// # Errors
/// Returns fixture construction or durable commit failures.
pub async fn seed_uncertain_terminal_siblings(
    database: &Path,
    source: SessionLocator,
    turn_id: TurnId,
) -> Result<SessionLocator, Box<dyn Error + Send + Sync>> {
    seed_terminal_debt(database, source, turn_id, true).await
}

#[allow(
    clippy::too_many_lines,
    reason = "the fixture constructs and commits one complete mixed-debt manifest"
)]
async fn seed_terminal_debt(
    database: &Path,
    source: SessionLocator,
    turn_id: TurnId,
    uncertain: bool,
) -> Result<SessionLocator, Box<dyn Error + Send + Sync>> {
    let store = SqliteSessionStore::open(database)?;
    let session_ref = SessionRef::new(source.session_id(), source.branch_id());
    let loaded = store.load_branch(session_ref).await?;
    let prior = loaded.checkpoint().checkpoint().manifest();
    if !prior.ledger().operations().iter().any(|record| {
            matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(id) } if *id == turn_id)
                && matches!(record.state(), DurableOperationState::Finalized(_))
        })
    {
        return Err("fixture requires a finalized root".into());
    }
    let lease = store
        .acquire(AcquireBranch::new(
            session_ref,
            RunId::try_from(Uuid::new_v4())?,
            NonZeroU64::new(30).ok_or("lease")?,
        ))
        .await?;
    let checkpoint_id = CheckpointId::try_from(Uuid::new_v4())?;
    let attempt_id = AttemptId::try_from(Uuid::new_v4())?;
    let call_id = CallId::try_from(Uuid::new_v4())?;
    let evidence = ReconstructiveComponent::new(
        RuntimeComponentKind::Operation,
        1,
        b"exact locally settled fixture sibling".as_slice(),
    )?;
    let first = prior
        .ledger()
        .operations()
        .iter()
        .map(|record| record.operation_id())
        .max()
        .ok_or("root operation")?
        + 1;
    let siblings = if uncertain {
        vec![
            (
                DurableOperationKind::Provider {
                    root: RuntimeOperationRoot::Turn(turn_id),
                    attempt_id,
                },
                RecoveryObligationKind::ProviderAttempt,
            ),
            (
                DurableOperationKind::Effect {
                    turn_id,
                    attempt_id,
                    call_id,
                    source_index: 2,
                },
                RecoveryObligationKind::Effect,
            ),
        ]
    } else {
        vec![
            (
                DurableOperationKind::Provider {
                    root: RuntimeOperationRoot::Turn(turn_id),
                    attempt_id,
                },
                RecoveryObligationKind::ProviderAttempt,
            ),
            (
                DurableOperationKind::CompositionEvaluation {
                    turn_id,
                    attempt_id,
                    call_id,
                    source_index: 0,
                },
                RecoveryObligationKind::CodeCell,
            ),
            (
                DurableOperationKind::Effect {
                    turn_id,
                    attempt_id,
                    call_id,
                    source_index: 2,
                },
                RecoveryObligationKind::Effect,
            ),
        ]
    };
    // Provider records carry provider-outcome-v2: the turn's own unknown
    // attempt's record, which recovery keeps when it finalizes that attempt.
    let mut provider_evidence = None;
    let mut ledger = prior.ledger().clone();
    for record in prior.ledger().operations() {
        // Seed a terminal sibling without abandoning the already-terminal root.
        if matches!(record.kind(), DurableOperationKind::Provider {
            root: RuntimeOperationRoot::Turn(id), ..
        } if *id == turn_id)
            && let DurableOperationState::OutcomeUnknown(outcome) = record.state()
        {
            provider_evidence = Some(outcome.clone());
            ledger = ledger.replace_operation(record.append_transition(
                DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(outcome.clone()),
                ),
            )?)?;
        }
    }
    let mut obligations = Vec::new();
    for record in ledger.operations() {
        if matches!(record.kind(), DurableOperationKind::Provider {
            root: RuntimeOperationRoot::Turn(id), ..
        } if *id == turn_id)
            && matches!(record.state(), DurableOperationState::Completed(_))
        {
            obligations.push(RecoveryObligation::new(
                record.operation_id(),
                RecoveryHandlerSignature::new(
                    RecoveryObligationKind::ProviderAttempt,
                    RecoveryAction::FinalizationRequired,
                ),
            ));
        }
    }
    for (index, (kind, obligation_kind)) in siblings.into_iter().enumerate() {
        let operation_id = first + u64::try_from(index)?;
        let mut transitions = vec![DurableOperationTransition::new(
            source.checkpoint_id(),
            DurableOperationState::Prepared,
        )];
        if uncertain {
            transitions.push(DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Dispatched,
            ));
        } else if obligation_kind == RecoveryObligationKind::Effect {
            transitions.push(DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::ProvenNotDispatched(evidence.clone()),
            ));
        } else {
            transitions.push(DurableOperationTransition::new(
                CheckpointId::try_from(Uuid::new_v4())?,
                DurableOperationState::Dispatched,
            ));
            transitions.push(DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Completed(
                    if obligation_kind == RecoveryObligationKind::ProviderAttempt {
                        provider_evidence
                            .clone()
                            .ok_or("fixture requires the turn's outcome-unknown provider attempt")?
                    } else {
                        evidence.clone()
                    },
                ),
            ));
        }
        let request = if matches!(kind, DurableOperationKind::CompositionEvaluation { .. }) {
            evaluation_request(call_id)?
        } else {
            evidence.clone()
        };
        ledger = ledger.append_operation(DurableOperationRecord::new(
            operation_id,
            kind,
            request,
            transitions,
        )?)?;
        obligations.push(RecoveryObligation::new(
            operation_id,
            RecoveryHandlerSignature::new(
                obligation_kind,
                if uncertain {
                    if obligation_kind == RecoveryObligationKind::ProviderAttempt {
                        RecoveryAction::ProviderEvidenceRequired
                    } else {
                        RecoveryAction::EffectEvidenceRequired
                    }
                } else {
                    RecoveryAction::FinalizationRequired
                },
            ),
        ));
    }
    let topology = prior.topology();
    let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision: topology.branch_revision + 1,
            lease_fence: lease.fence().get(),
            commit_identity: Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"fixture-debt"],
            )?,
            ..topology
        },
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn {
                turn_id,
                next_round: 1,
            },
            recovery_phase: if uncertain {
                WorkingRecoveryPhase::EffectReconciliation
            } else {
                WorkingRecoveryPhase::Finalization
            },
            obligations,
        },
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?;
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, loaded.checkpoint().bundle())?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), loaded.head(), bundle))
        .await?;
    store.release(lease).await?;
    Ok(SessionLocator::new(
        source.session_id(),
        source.branch_id(),
        checkpoint_id,
    ))
}

/// Seeds a pending `RecoveryRequired` root with one uncertain fenced Code Cell.
///
/// # Errors
/// Returns typed fixture construction or commit failures.
#[allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    reason = "the fixture writes one complete exact ledger cut"
)]
pub async fn seed_fenced_code_cell(
    database: &Path,
    source: SessionLocator,
    command: &str,
    input: &[u8],
    finalized_root: bool,
) -> Result<SessionLocator, Box<dyn Error + Send + Sync>> {
    use plakous_agent::internal::{DurableCommandDisposition, DurableCommandRecord};
    let store = SqliteSessionStore::open(database)?;
    let loaded = store
        .load_branch(SessionRef::new(source.session_id(), source.branch_id()))
        .await?;
    let prior = loaded.checkpoint().checkpoint().manifest();
    let lease = store
        .acquire(AcquireBranch::new(
            loaded.head().session(),
            RunId::try_from(Uuid::new_v4())?,
            NonZeroU64::new(30).ok_or("lease")?,
        ))
        .await?;
    let checkpoint_id = CheckpointId::try_from(Uuid::new_v4())?;
    let turn_id = TurnId::try_from(Uuid::new_v4())?;
    let first = prior
        .ledger()
        .operations()
        .iter()
        .map(|record| record.operation_id())
        .max()
        .unwrap_or(0)
        + 1;
    let request = serde_json::to_vec(
        &serde_json::json!({"version":1, "canonical_input":input, "effect":"Root", "rejection":null}),
    )?;
    let command_record = DurableCommandRecord::new(
        command,
        prior
            .ledger()
            .commands()
            .last()
            .map_or(1, |record| record.sequence() + 1),
        Some(turn_id),
        Digest::sha256(DigestDomain::Blob, 1, &[input])?,
        ReconstructiveComponent::new(RuntimeComponentKind::Command, 1, request.clone())?,
        DurableCommandDisposition::Accepted,
        None,
    )?;
    #[derive(serde::Serialize)]
    struct Pending {
        remedy: Option<serde_json::Value>,
        version: u32,
        command_id: String,
        turn_id: TurnId,
        operation_id: u64,
        outcome: Outcome,
        usage: plakous::agent::TurnUsage,
    }
    #[derive(serde::Serialize)]
    enum Outcome {
        RecoveryRequired(ErrorRecord),
    }
    #[derive(serde::Serialize)]
    struct ErrorRecord {
        kind: String,
        message: String,
        model_error: Option<serde_json::Value>,
        tool_kind: Option<String>,
        store_error: Option<serde_json::Value>,
    }
    let pending = Pending {
        remedy: None,
        version: 2,
        command_id: command.into(),
        turn_id,
        operation_id: first,
        outcome: Outcome::RecoveryRequired(ErrorRecord {
            kind: "OutcomeUnknown".into(),
            message: "fenced fixture cell".into(),
            model_error: None,
            tool_kind: None,
            store_error: None,
        }),
        usage: plakous::agent::TurnUsage::default(),
    };
    let mut root = DurableOperationRecord::new(
        first,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, request)?,
        vec![
            DurableOperationTransition::new(
                source.checkpoint_id(),
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(Uuid::new_v4())?,
                DurableOperationState::Dispatched,
            ),
            DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Completed(ReconstructiveComponent::new(
                    RuntimeComponentKind::Operation,
                    1,
                    serde_json::to_vec(&("plakous-terminal-result-pending", &pending))?,
                )?),
            ),
        ],
    )?;
    if finalized_root {
        #[derive(serde::Serialize)]
        struct Terminal<'a> {
            remedy: Option<serde_json::Value>,
            abandonment: Option<serde_json::Value>,
            version: u32,
            command_id: &'a str,
            turn_id: TurnId,
            operation_id: u64,
            checkpoint_id: CheckpointId,
            conversation_identity: Digest,
            outcome: &'a Outcome,
            usage: plakous::agent::TurnUsage,
        }
        let terminal = Terminal {
            remedy: None,
            abandonment: None,
            version: 2,
            command_id: command,
            turn_id,
            operation_id: first,
            checkpoint_id,
            conversation_identity: prior.canonical().history().identity(),
            outcome: &pending.outcome,
            usage: pending.usage,
        };
        root = root.append_transition(DurableOperationTransition::new(
            checkpoint_id,
            DurableOperationState::Finalized(ReconstructiveComponent::new(
                RuntimeComponentKind::Operation,
                1,
                serde_json::to_vec(&("plakous-terminal-result", terminal))?,
            )?),
        ))?;
    }
    let call_id = CallId::try_from(Uuid::new_v4())?;
    let cell = DurableOperationRecord::new(
        first + 1,
        DurableOperationKind::CompositionEvaluation {
            turn_id,
            attempt_id: AttemptId::try_from(Uuid::new_v4())?,
            call_id,
            source_index: 0,
        },
        evaluation_request(call_id)?,
        vec![
            DurableOperationTransition::new(
                source.checkpoint_id(),
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(Uuid::new_v4())?,
                DurableOperationState::Dispatched,
            ),
        ],
    )?;
    let ledger = prior
        .ledger()
        .append_command(command_record)?
        .append_operation(root)?
        .append_operation(cell)?;
    let mut obligations = Vec::new();
    if !finalized_root {
        obligations.push(RecoveryObligation::new(
            first,
            RecoveryHandlerSignature::new(
                RecoveryObligationKind::Command,
                RecoveryAction::FinalizationRequired,
            ),
        ));
    }
    obligations.push(RecoveryObligation::new(
        first + 1,
        RecoveryHandlerSignature::new(
            RecoveryObligationKind::CodeCell,
            RecoveryAction::EvaluationAbandonmentRequired,
        ),
    ));
    let topology = prior.topology();
    let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision: topology.branch_revision + 1,
            lease_fence: lease.fence().get(),
            commit_identity: Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"fenced-fixture"],
            )?,
            ..topology
        },
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn {
                turn_id,
                next_round: 1,
            },
            recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
            obligations,
        },
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?;
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, loaded.checkpoint().bundle())?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), loaded.head(), bundle))
        .await?;
    store.release(lease).await?;
    Ok(SessionLocator::new(
        source.session_id(),
        source.branch_id(),
        checkpoint_id,
    ))
}

fn evaluation_request(
    call_id: CallId,
) -> Result<ReconstructiveComponent, Box<dyn Error + Send + Sync>> {
    use plakous::model::{
        EntryId, ToolCallBlock, ToolCallState, ToolCallerLineage, ToolCorrelation,
        ToolExecutionOwner, ToolName,
    };

    #[derive(serde::Serialize)]
    struct EvaluationRequest {
        version: u32,
        descriptor_identity: Digest,
        host_identity: Vec<u8>,
        call: ToolCallBlock,
        arguments_digest: Digest,
        source_digest: Option<Digest>,
        result_entry_id: EntryId,
    }
    let identity = Digest::sha256(DigestDomain::ProviderOperation, 3, &[b"fixture-code-cell"])?;
    let request = EvaluationRequest {
        version: 2,
        descriptor_identity: identity,
        host_identity: b"fixture-host".to_vec(),
        call: ToolCallBlock::new(
            call_id,
            ToolName::new("exec")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(None, None, ToolCallerLineage::Direct)?,
            "text('settled')",
        )?,
        arguments_digest: identity,
        source_digest: None,
        result_entry_id: EntryId::try_from(Uuid::new_v4())?,
    };
    Ok(ReconstructiveComponent::new(
        RuntimeComponentKind::Operation,
        1,
        serde_json::to_vec(&("plakous-local-composition-evaluation-v2", request))?,
    )?)
}
