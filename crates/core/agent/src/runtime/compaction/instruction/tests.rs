use super::*;
use crate::runtime::compaction::record::compaction_operation_component;
use crate::{DurableOperationTransition, RuntimeComponentKind, SessionWorkflowId};
use plakous_model::{
    AttemptId, CanonicalHistory, CheckpointId, Digest, DigestDomain, FinishDetails, ModelRequest,
    RequestMetadata, RunId, Timestamp,
};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn request(workflow_id: SessionWorkflowId) -> TestResult<DurableCompactionRequestRecord> {
    let checkpoint = crate::checkpoint_bundle::tests::fixture_with_history(
        CanonicalHistory::new(Vec::new())?,
        10,
        false,
    )?;
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    let digest = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"instruction-selection"])?;
    Ok(DurableCompactionRequestRecord {
        version: 2,
        workflow_id,
        turn_fence: false,
        operation_id: "instruction-selection".to_owned(),
        source_session: topology.session_id,
        source_branch: topology.branch_id,
        source_checkpoint: checkpoint.id(),
        source_history: manifest.canonical().history().identity(),
        cut: digest,
        request: ModelRequest::from_parts(
            manifest.canonical().stable_prefix().clone(),
            manifest.canonical().projection(),
            RequestMetadata::for_session_workflow(
                RunId::try_from(Uuid::from_u128(11))?,
                workflow_id,
                AttemptId::try_from(Uuid::from_u128(12))?,
                Timestamp::new(1, 0)?,
            ),
        )?,
        provider_configuration: manifest.provider_configuration().clone(),
        source_harness_plan: digest,
        before_measurement: digest,
        before_status: digest,
        before_input_tokens: 1,
        target_affinity_entropy: [0; 32],
    })
}

#[test]
fn latest_harness_fold_requires_finalized_compaction_record() -> TestResult {
    let workflow = SessionWorkflowId::try_from([1; 32])?;
    let request = request(workflow)?;
    let checkpoint = request.source_checkpoint;
    let dispatched = CheckpointId::try_from(Uuid::from_u128(14))?;
    let completed = CheckpointId::try_from(Uuid::from_u128(15))?;
    let finalization = CheckpointId::try_from(Uuid::from_u128(16))?;
    let response = compaction_operation_component(&ModelResponse::new(
        Vec::new(),
        FinishDetails::new(FinishReason::Length, None),
        None,
        None,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"response"])?,
    ))?;
    assert_eq!(response.kind(), RuntimeComponentKind::Operation);
    let transitions = vec![
        DurableOperationTransition::new(checkpoint, DurableOperationState::Prepared),
        DurableOperationTransition::new(dispatched, DurableOperationState::Dispatched),
        DurableOperationTransition::new(
            completed,
            DurableOperationState::Completed(response.clone()),
        ),
    ];
    let mut finalized = transitions.clone();
    finalized.push(DurableOperationTransition::new(
        finalization,
        DurableOperationState::Finalized(response.clone()),
    ));
    let ledger = RuntimeOperationLedger::new(
        Vec::new(),
        vec![
            DurableOperationRecord::new(
                1,
                DurableOperationKind::Compaction {
                    workflow_id: workflow,
                },
                compaction_operation_component(&request)?,
                transitions,
            )?,
            DurableOperationRecord::new(
                2,
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::SessionWorkflow(workflow),
                    attempt_id: AttemptId::try_from(Uuid::from_u128(12))?,
                },
                compaction_operation_component(&request)?,
                finalized,
            )?,
        ],
    )?;
    assert!(latest_harness_fold(&ledger).is_none());
    let ledger = ledger.transition_operation(
        1,
        DurableOperationTransition::new(finalization, DurableOperationState::Finalized(response)),
    )?;
    let (compaction, provider, finish) =
        latest_harness_fold(&ledger).ok_or("finalized pair missing")?;
    assert_eq!(
        (compaction.operation_id(), provider.operation_id(), finish),
        (1, 2, FinishReason::Length)
    );
    Ok(())
}

#[test]
fn tight_instruction_pin() {
    let text = CompactionInstruction::Tight.text();
    assert_eq!(text.len(), 2057);
    assert_eq!(
        plakous_model::sha256_hex(&[text.as_bytes()]),
        "4a2a372567a48ec74ed0c4a272023d179117ada0e8d5e7f0727c813ba38f9b6b"
    );
}
