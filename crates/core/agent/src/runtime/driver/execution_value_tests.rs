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

fn registry(effect: ToolEffect, attach: bool, images: bool) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("value_probe")?,
        "1.0.0",
        "Return fixed text and an optional value.",
        ToolInputContract::utf8(100)?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        effect,
    )?;
    let mut parts = ToolOutcome::success_text("unchanged")?.parts().to_vec();
    if images {
        parts.push(plakous_model::ToolResultPart::Image(
            plakous_model::ImageBlock::new(
                "image/png",
                plakous_model::MediaSource::data_url("data:image/png;base64,AQID")?,
            )?,
        ));
    }
    let outcome = ToolOutcome::success(parts)?;
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
        abandonment: Mutex::default(),
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

async fn execute(
    effect: ToolEffect,
    attach: bool,
    images: bool,
) -> TestResult<ToolExecutionResult> {
    execute_controlled(effect, attach, images, None).await
}

async fn execute_controlled(
    effect: ToolEffect,
    attach: bool,
    images: bool,
    image_control: Option<(Arc<dyn plakous_tools::ImageNormalizer>, CancellationToken)>,
) -> TestResult<ToolExecutionResult> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let prepared = crate::runtime::durable::tests::execution_value_fixture(
        registry(effect, attach, images)?,
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
        let (environment, cancellation) =
            image_environment(prepared.tools.environment(), image_control);
        return Ok(crate::runtime::driver::effects::present_effect_result(
            &mut outcome,
            &prepared.tool_presentation,
            None,
            &invocation,
            &call,
            true,
            (&environment, &cancellation),
        )
        .await?);
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

fn image_environment(
    environment: &plakous_tools::ExecutionEnvironment,
    control: Option<(Arc<dyn plakous_tools::ImageNormalizer>, CancellationToken)>,
) -> (plakous_tools::ExecutionEnvironment, CancellationToken) {
    match control {
        Some((normalizer, cancellation)) => (
            plakous_tools::internal::install_visual_read(
                environment.clone(),
                "test",
                "test",
                true,
                Some(normalizer),
            ),
            cancellation,
        ),
        None => (environment.clone(), CancellationToken::new()),
    }
}

#[tokio::test]
async fn supervised_read_only_and_claimed_effect_keep_values_out_of_evidence() -> TestResult {
    for effect in [ToolEffect::ReadOnly, ToolEffect::Mutating] {
        let typed = Box::pin(execute(effect, true, false)).await?;
        assert_eq!(
            typed.value,
            Some(ExecutionValue::Plan(PlanValue { revision: 7 }))
        );
        let plain = Box::pin(execute(effect, false, false)).await?;
        assert!(plain.value.is_none());
        let entry = EntryId::try_from(uuid::Uuid::from_u128(9304))?;
        assert_eq!(
            completed_tool_outcome_component(&Ok(typed), entry)?.bytes(),
            completed_tool_outcome_component(&Ok(plain), entry)?.bytes(),
        );
    }
    Ok(())
}

#[tokio::test]
async fn readonly_and_mutating_image_results_share_the_route_gate() -> TestResult {
    for effect in [ToolEffect::ReadOnly, ToolEffect::Mutating] {
        let result = Box::pin(execute(effect, false, true)).await?;
        let [plakous_model::ToolResultPart::Text(text)] = result.result.parts() else {
            return Err("a tool image escaped route admission".into());
        };
        assert_eq!(
            text.text(),
            "unchanged\n[1 image(s) omitted: this tool's result carries text only here]"
        );
    }
    Ok(())
}

struct CancellingNormalizer {
    cancellation: CancellationToken,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl plakous_tools::ImageNormalizer for CancellingNormalizer {
    fn normalize(&self, bytes: &[u8]) -> Result<plakous_tools::PreparedImage, String> {
        self.cancellation.cancel();
        self.release
            .lock()
            .map_err(|_| "release lock")?
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "release timeout")?;
        Ok(plakous_tools::PreparedImage::new(
            "image/png",
            bytes.to_vec(),
        ))
    }
}

#[tokio::test]
async fn cancellation_after_mutating_effect_keeps_completed_evidence() -> TestResult {
    let cancellation = CancellationToken::new();
    let (release, receiver) = std::sync::mpsc::channel();
    let normalizer = Arc::new(CancellingNormalizer {
        cancellation: cancellation.clone(),
        release: Mutex::new(receiver),
    });
    let result = execute_controlled(
        ToolEffect::Mutating,
        false,
        true,
        Some((normalizer, cancellation)),
    )
    .await;
    release.send(())?;
    let execution = result?;
    assert_eq!(
        execution.result.kind(),
        plakous_model::ToolResultKind::Success
    );
    let [plakous_model::ToolResultPart::Text(text)] = execution.result.parts() else {
        return Err("completed effect retained an image".into());
    };
    assert_eq!(
        text.text(),
        "unchanged\n[1 image(s) omitted: normalization cancelled after tool completed]"
    );
    let entry = EntryId::try_from(uuid::Uuid::from_u128(9305))?;
    let state = crate::runtime::operation_record::tool_operation_terminal_state(
        &Ok(execution),
        true,
        entry,
    )?;
    let crate::manifest::DurableOperationState::Completed(evidence) = state else {
        return Err("effect was not recorded completed".into());
    };
    let value: serde_json::Value = serde_json::from_slice(evidence.bytes())?;
    assert_eq!(value[0], "completed-tool-outcome-v1");
    assert!(value.to_string().contains("unchanged"));
    Ok(())
}

#[tokio::test]
async fn prepared_view_image_keeps_high_detail_after_present_outcome() -> TestResult {
    use plakous_model::{ImageBlock, ImageDetailHint, MediaSource, ToolResultPart};
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let prepared = crate::runtime::durable::tests::execution_value_fixture(
        registry(ToolEffect::ReadOnly, false, true)?,
        workspace,
    )?;
    let invocation = prepared.tools.prepare(ToolInvocationRequest::new(
        prepared.run_id,
        TurnId::try_from(uuid::Uuid::from_u128(9401))?,
        AttemptId::try_from(uuid::Uuid::from_u128(9402))?,
        CallId::try_from(uuid::Uuid::from_u128(9403))?,
        ToolName::new("value_probe")?,
        ToolCorrelation::default(),
        "input",
    ))?;
    let normalizer = Arc::new(crate::runtime::durable::tests::FixtureImageNormalizer);
    let environment = plakous_tools::internal::install_visual_read(
        prepared.tools.environment().as_ref().clone(),
        "test",
        "test",
        true,
        Some(normalizer),
    );
    let outcome = ToolOutcome::success(vec![ToolResultPart::Image(ImageBlock::with_detail(
        "image/png",
        MediaSource::bytes(vec![1, 2, 3])?,
        Some(ImageDetailHint::High),
    )?)])?;
    let (result, _) = crate::runtime::ToolPresentation::present_outcome(
        outcome,
        &invocation,
        &environment,
        &CancellationToken::new(),
    )
    .await?;
    let [ToolResultPart::Image(image)] = result.parts() else {
        return Err("view image omitted".into());
    };
    assert_eq!(image.detail(), Some(ImageDetailHint::High));
    Ok(())
}
