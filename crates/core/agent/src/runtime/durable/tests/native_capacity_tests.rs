use super::*;

fn reference_input_descriptor(
    native: bool,
    rule: CapacityRule,
) -> TestResult<ResolvedModelDescriptor> {
    let descriptor = placeholder_test_descriptor(272_000, native)?;
    let authority_binding = artifact_authority(&descriptor, 1)?.authority_class_binding()?;
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, Some(Timestamp::new(20, 0)?))?;
    let receipt = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[b"driver-reference-catalog"],
    )?;
    let contract = ContextContract::new(
        descriptor.protocol().clone(),
        "artifact-accounting-v1",
        "artifact-projection-v1",
        "artifact-reservation-v1",
        CapacityCertificate::reference_client_envelope(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "agent-scripted-capacity",
            "driver-reference-v1",
            rule,
            authority_binding,
            freshness,
            receipt,
            "driver-reference-v1",
        )?,
        vec![MeasurementCertificate::standard(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "agent-scripted-counter",
            "agent-scripted-exact-v1",
            "agent-scripted-complete-projection",
            "agent-scripted-method-v1",
            ContextEvidenceSource::AuthenticatedDiscovery,
            CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                catalog_receipt: receipt,
            },
            Some(authority_binding),
            freshness,
        )?],
    )?;
    Ok(descriptor.with_context_contract(contract))
}

async fn full_codex_sized_history(
    native: bool,
    entry_base: u128,
) -> TestResult<(tempfile::TempDir, Agent, Arc<ArtifactModel>)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = reference_input_descriptor(
        native,
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(272_000),
            maximum_projected_input: 258_400,
            auto_compact_input: Some(244_800),
        },
    )?;
    let (builder, model) = placeholder_fixture_with_descriptor(
        workspace,
        entry_base,
        descriptor,
        &[],
        &[],
        LocalDurabilityDisposition::Ephemeral,
        0,
        &[],
    )?;
    let agent = builder.build()?;
    for prompt in [
        "older intent ".repeat(10_000),
        "latest intent ".repeat(10_000),
    ] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    let source_tokens = agent
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .retained_bytes_upper_bound()
        .div_ceil(4);
    if native {
        let multiplier = 258_400 / source_tokens + 1;
        model
            .context_token_multiplier
            .store(multiplier, Ordering::Release);
        assert!(source_tokens * multiplier > 258_400);
        assert!(source_tokens * multiplier < 272_000);
    } else {
        // The ordinary envelope overflows, but both the old and new sealed
        // summary cuts admit: an unpaid failure can legally recover after intent.
        model
            .ordinary_context_overhead
            .store(258_400 - source_tokens + 4_096, Ordering::Release);
    }
    Ok((directory, agent, model))
}

#[tokio::test]
async fn native_capacity_preparation_refusal_uses_the_sealed_harness_cut() -> TestResult {
    let (_directory, agent, model) = full_codex_sized_history(true, 343_000).await?;
    let result = agent.prompt("short follow-up").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}; diagnostic={}; paid={}; contexts={}",
        result.outcome(),
        match result.outcome() {
            TurnResultOutcome::Failed(error) => error.message(),
            _ => "none",
        },
        model.paid_compaction_calls.load(Ordering::Acquire),
        model.observed_compaction_contexts().len()
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("fold")?
            .strategy(),
        RuntimeCompactionStrategy::SemanticSummaryPrefixV1
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn unpaid_admission_fold_failure_preserves_overflow_recovery() -> TestResult {
    let (_directory, agent, model) = full_codex_sized_history(false, 344_000).await?;
    *lock_mutex(&model.reject_compaction_calls) = BTreeSet::from([1]);
    let result = agent
        .prompt("new intent makes a smaller retained tail")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    assert_eq!(lock_mutex(&model.compaction_start_rounds).len(), 2);
    assert!(
        agent
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .iter()
            .any(|record| {
                record.transitions().iter().any(|transition| {
                    matches!(
                        transition.state(),
                        DurableOperationState::ProvenNotDispatched(_)
                    )
                })
            })
    );
    agent.shutdown().await?;
    Ok(())
}

#[test]
fn native_scheduling_holds_descriptor_room_without_changing_admission() -> TestResult {
    use crate::runtime::compaction::request::{
        native_compaction_scheduling_reserve, tool_group_fence_verdict,
    };
    use plakous_model::{ContextHeadroom, ContextStatusKind, QualifiedTokens};

    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    for (window, ceiling, expected) in [
        (Some(500_000), 475_000, 25_000),
        (Some(272_000), 258_400, 13_600),
        (None, 475_000, 0),
    ] {
        for native in [false, true] {
            let descriptor = reference_input_descriptor(
                native,
                CapacityRule::ProviderReservedInput {
                    reported_context_window: window,
                    maximum_projected_input: ceiling,
                    auto_compact_input: None,
                },
            )?;
            let (builder, _) = placeholder_fixture_with_descriptor(
                workspace.clone(),
                820_000,
                descriptor,
                &[],
                &[],
                LocalDurabilityDisposition::Ephemeral,
                0,
                &[],
            )?;
            let reserve = native_compaction_scheduling_reserve(&builder.provider_configuration);
            assert_eq!(reserve, if native { expected } else { 0 });
            let make_fit = |headroom| ContextStatusKind::Fits {
                headroom: ContextHeadroom::ProviderReservedInput {
                    input: QualifiedTokens::Exact(headroom),
                },
            };
            assert!(
                !tool_group_fence_verdict(&make_fit(reserve + 4097), None, None, false, reserve)
                    .is_due()
            );
            assert!(
                tool_group_fence_verdict(&make_fit(reserve + 4096), None, None, false, reserve)
                    .observation_headroom()
            );
            assert!(!tool_group_fence_verdict(&make_fit(0), None, None, true, reserve).is_due());
            // Scheduling never changes the descriptor's admitted input envelope.
            assert_eq!(builder.session_input_window(), Some((window, ceiling)));
        }
    }
    let (builder, _) = placeholder_fixture(
        workspace,
        821_000,
        500_000,
        &[],
        &[],
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    assert_eq!(
        native_compaction_scheduling_reserve(&builder.provider_configuration),
        0
    );
    Ok(())
}

#[tokio::test]
async fn native_request_room_schedules_before_the_next_prompt() -> TestResult {
    Box::pin(native_request_room_case(false)).await
}

#[tokio::test]
async fn native_request_room_schedules_after_a_tool_group() -> TestResult {
    Box::pin(native_request_room_case(true)).await
}

async fn native_request_room_case(tool_group: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = reference_input_descriptor(
        true,
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(272_000),
            maximum_projected_input: 258_400,
            auto_compact_input: Some(244_800),
        },
    )?;
    let rounds: &[usize] = if tool_group { &[2] } else { &[] };
    let bodies: &[usize] = if tool_group { &[100] } else { &[] };
    let (builder, model) = placeholder_fixture_with_descriptor(
        workspace,
        830_000,
        descriptor,
        bodies,
        rounds,
        LocalDurabilityDisposition::Ephemeral,
        0,
        &[],
    )?;
    let reserve = crate::runtime::compaction::request::native_compaction_scheduling_reserve(
        &builder.provider_configuration,
    );
    model
        .native_context_overhead
        .store(usize::try_from(reserve)?, Ordering::Release);
    let agent = builder.build()?;
    for prompt in ["older requirement", "latest requirement"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
    // Below the catalog's occupancy trigger, above the old 4096 reserve,
    // but within one native request's own descriptor-derived room.
    let overhead = if tool_group {
        &model.tool_successor_capacity_loss
    } else {
        &model.ordinary_context_overhead
    };
    overhead.store(241_000, Ordering::Release);
    let result = agent
        .prompt("continue with room to compact")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    assert_eq!(model.ordinary_capacity_refusals.load(Ordering::Acquire), 0);
    let checkpoint = agent.checkpoint();
    assert_eq!(
        checkpoint
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("native provenance")?
            .strategy(),
        RuntimeCompactionStrategy::NativeResponsesTriggerV1
    );
    // ProviderReservedInput remains input-only even on the native request.
    {
        let records = lock_mutex(&model.prepared_records);
        let observed = lock_mutex(&model.observed_provenance);
        for (record, (plan, context)) in records.iter().zip(observed.iter()) {
            let status = model.descriptor.context_status(
                plan,
                record.projection(),
                context.prepared_at(),
            )?;
            assert!(status.reservation().is_none());
        }
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn paid_admission_fold_failure_reports_the_ladder_without_redispatch() -> TestResult {
    let (_directory, agent, model) = full_codex_sized_history(false, 347_000).await?;
    model.fail_compaction_call();
    let before = model.calls.load(Ordering::Acquire);
    let result = agent.prompt("short follow-up").await?.result().await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        panic!(
            "paid fold failure admitted overflowing input: {:?}",
            result.outcome()
        );
    };
    assert_eq!(error.model_kind(), Some(ModelErrorKind::ContextLimit));
    assert!(
        error
            .model_error()
            .and_then(ModelError::context_capacity)
            .is_some()
    );
    for field in ["rung=", "outcome=", "refusal="] {
        assert!(error.message().contains(field), "{}", error.message());
    }
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), before + 1);
    agent.shutdown().await?;
    Ok(())
}
