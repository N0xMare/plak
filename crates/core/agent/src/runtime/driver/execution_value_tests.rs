//! Typed-value transport through supervised read-only and claimed-effect execution.

use super::*;
use crate::runtime::operation_record::completed_tool_outcome_component;
use crate::runtime::{TurnLifecycle, TurnSettlement, TurnUsage};
use crate::tool_runtime::EffectAuthorization;
use plakous_model::{
    Digest, DigestDomain, ToolCallBlock, ToolCallState, ToolConcurrency, ToolCorrelation,
    ToolDefinition, ToolEffect, ToolExecutionOwner, ToolInputContract, ToolName,
    ToolOutputContract,
};
use plakous_tools::{
    ExecutionValue, FrozenToolRegistry, PlanValue, RegisteredToolId, ToolExecutorBindingIdentity,
    ToolExecutorId, ToolInvocationRequest, ToolOutcome, ToolRegistration, ToolRegistryBuilder,
    WorkspacePolicy,
};
use std::sync::atomic::AtomicU64;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn registry(effect: ToolEffect, attach: bool) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("value_probe")?,
        "1.0.0",
        "Return fixed text and an optional value.",
        ToolInputContract::utf8(100)?,
        ToolOutputContract::utf8(100)?,
        ToolConcurrency::Serial,
        effect,
    )?;
    let outcome = ToolOutcome::success_text("unchanged")?;
    let outcome = if attach {
        outcome.with_value(ExecutionValue::Plan(PlanValue { revision: 7 }))?
    } else {
        outcome
    };
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("value.probe")?,
        ToolExecutorId::new("value.probe")?,
        ToolExecutorBindingIdentity::new(1, &[b"value.probe"])?,
        definition,
        Arc::new(plakous_test_support::ScriptedTool::new([Ok(outcome)])),
    ))?;
    Ok(registry.freeze())
}

fn shared(turn_id: TurnId, events: EventPipeline) -> TestResult<Arc<TurnShared>> {
    Ok(Arc::new(TurnShared {
        storage_pause: crate::runtime::checkpoint::storage_pause::StoragePause::default(),
        turn_id,
        lifecycle: TurnLifecycle::Admitted(Mutex::new(
            crate::runtime::terminal::queued_turn_machine(turn_id)?,
        )),
        settlement: tokio::sync::watch::channel(TurnSettlement::Pending).0,
        cancellation: CancellationToken::new(),
        result: tokio::sync::watch::channel(None).0,
        terminal_failure: tokio::sync::watch::channel(None).0,
        events,
        usage: Mutex::new(TurnUsage::default()),
        telemetry: None,
        terminal_fault: AtomicU64::new(0),
        published_results: AtomicU64::new(0),
        binding_lookup_fault: AtomicBool::new(false),
    }))
}

async fn execute(effect: ToolEffect, attach: bool) -> TestResult<ToolExecutionResult> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let prepared = crate::runtime::durable::tests::execution_value_fixture(
        registry(effect, attach)?,
        workspace,
    )?;
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(9301))?;
    let attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(9302))?;
    let call_id = CallId::try_from(uuid::Uuid::from_u128(9303))?;
    let invocation = prepared.tools.prepare(ToolInvocationRequest::new(
        prepared.run_id,
        turn_id,
        attempt_id,
        call_id,
        ToolName::new("value_probe")?,
        ToolCorrelation::default(),
        "input",
    ))?;
    let call = ToolCallBlock::new(
        call_id,
        ToolName::new("value_probe")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        serde_json::to_string("input")?,
    )?;
    let EffectAuthorization::Allowed(authorized) =
        prepared.tools.authorize(invocation.clone()).await?
    else {
        return Err("probe authorization refused".into());
    };
    let events = EventPipeline::new(prepared.events);
    let shared = shared(turn_id, events.clone())?;
    if effect != ToolEffect::ReadOnly {
        let intent = prepared.tools.effect_dispatch_intent(&authorized)?;
        let claim = Digest::sha256(DigestDomain::Snapshot, 1, &[b"value probe claim"])?;
        let claimed = prepared.tools.claim_effect(authorized, intent, claim)?;
        let (progress, _receiver) = ProgressSink::channel(4, call_id, Arc::clone(&prepared.clock))?;
        let control =
            ToolExecutionControl::new(CancellationToken::new(), None, prepared.clock, progress);
        let mut outcome = Box::pin(prepared.tools.execute_claimed(claimed, control))
            .await
            .into_result();
        return Ok(crate::runtime::driver::effects::present_effect_result(
            &mut outcome,
            &prepared.tool_presentation,
            None,
            &invocation,
            &call,
            true,
        )?);
    }
    let authority = prepared.authority.start();
    Ok(Box::pin(execute_read_only_tool(IndexedReadOnlyRequest {
        authority: authority.start_authority(CancellationToken::new()),
        operation_id: crate::runtime::OperationId(1),
        tools: prepared.tools,
        tool_presentation: prepared.tool_presentation,
        audit_identity: ToolAuditIdentity::from_invocation(&invocation),
        lineage: ToolCallerLineageView::direct(),
        invocation,
        projection_receipt: None,
        authorized,
        index: 0,
        call,
        result_entry_id: EntryId::try_from(uuid::Uuid::from_u128(9304))?,
        cancellation: CancellationToken::new(),
        shutdown: CancellationToken::new(),
        preempt: tokio::sync::watch::channel(plakous_tools::ToolPreempt::None).1,
        preempt_guard: None,
        clock: prepared.clock,
        progress_capacity: 4,
        events,
        turn_id,
        attempt_id,
        telemetry: prepared.telemetry,
        telemetry_context: prepared.telemetry_context,
        turn_shared: shared,
        dispatch_witness: Arc::new(AtomicBool::new(false)),
        process_session_update: Arc::new(Mutex::new(None)),
        access_salt: None,
    }))
    .await?)
}

#[tokio::test]
async fn supervised_read_only_and_claimed_effect_keep_values_out_of_evidence() -> TestResult {
    for effect in [ToolEffect::ReadOnly, ToolEffect::Mutating] {
        let typed = Box::pin(execute(effect, true)).await?;
        assert_eq!(
            typed.value,
            Some(ExecutionValue::Plan(PlanValue { revision: 7 }))
        );
        let plain = Box::pin(execute(effect, false)).await?;
        assert!(plain.value.is_none());
        let entry = EntryId::try_from(uuid::Uuid::from_u128(9304))?;
        assert_eq!(
            completed_tool_outcome_component(&Ok(typed), entry)?.bytes(),
            completed_tool_outcome_component(&Ok(plain), entry)?.bytes(),
        );
    }
    Ok(())
}
