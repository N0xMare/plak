use super::*;

#[tokio::test]
async fn byte_measured_read_clip_fits_the_next_ephemeral_dispatch() -> TestResult {
    clipped_read_fits_next_dispatch(LocalDurabilityDisposition::Ephemeral).await
}

#[tokio::test]
async fn byte_measured_read_clip_fits_the_next_durable_dispatch() -> TestResult {
    clipped_read_fits_next_dispatch(LocalDurabilityDisposition::DurableRequired).await
}

async fn byte_measured_read_fixture(
    durability: LocalDurabilityDisposition,
    scheduled: bool,
) -> TestResult<(tempfile::TempDir, Agent, Arc<ArtifactModel>)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = custom_artifact_test_descriptor("test.route")?;
    let measurement = descriptor
        .context_contract()
        .ok_or("contract")?
        .measurements()[0]
        .clone();
    let CountCertificateKind::CustomEnvelopeUpperBound { declaration } = measurement.kind() else {
        return Err("custom measurement required".into());
    };
    let capacity = CapacityCertificate::custom_envelope(
        "byte-clip-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(65_536),
            maximum_projected_input: 32_768,
            auto_compact_input: None,
        },
        measurement.authority_binding().ok_or("authority")?,
        measurement.freshness(),
        declaration,
        "v1",
    )?;
    let contract = ContextContract::new(
        descriptor.protocol().clone(),
        "artifact-accounting-v1",
        "artifact-projection-v1",
        "artifact-reservation-v1",
        capacity,
        vec![measurement],
    )?;
    let (builder, model) = placeholder_fixture_with_descriptor(
        workspace,
        120_000,
        descriptor.with_context_contract(contract),
        &[40_000],
        &[0],
        durability,
        0,
        &[],
    )?;
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(19_000, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(4_651, Ordering::Release);
    let builder = builder.with_scheduled_compaction_enabled(scheduled);
    if durability == LocalDurabilityDisposition::Ephemeral {
        return Ok((directory, builder.build()?, model));
    }
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    Ok((
        directory,
        sessions.create("create:byte-clip", builder).await?,
        model,
    ))
}

async fn clipped_read_fits_next_dispatch(durability: LocalDurabilityDisposition) -> TestResult {
    let (_directory, agent, model) = byte_measured_read_fixture(durability, false).await?;
    let mut turn = agent.prompt("read the large file").await?;
    let mut events = turn.take_events()?;
    let mut bounds = Vec::new();
    loop {
        match events.recv().await?.kind() {
            AgentEventKind::RequestPrepared {
                prepared_input_upper_bound,
                ..
            } => {
                bounds.push(prepared_input_upper_bound.ok_or("missing admission bound")?);
            }
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(bounds.len(), 2);
    assert!(bounds.iter().all(|bound| *bound <= 32_768), "{bounds:?}");
    let records = model.prepared_records();
    assert_eq!(records.len(), 3, "admission probe and two provider rounds");
    assert!(
        records[2]
            .projection()
            .token_projection()
            .request_anchor()
            .is_some()
    );
    // Whole-wire size is not the gate when a matching item-delta anchor fits.
    let input = serde_json::to_value(
        records[2]
            .projection()
            .token_projection()
            .wire_input()
            .ok_or("wire input")?,
    )?;
    let whole_wire: u64 = input["items"]
        .as_array()
        .ok_or("wire items")?
        .iter()
        .map(|item| item["bytes"].as_u64().unwrap_or(0) + 16)
        .sum();
    assert!(whole_wire > 32_768, "{whole_wire}");
    assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks)
        .any(|block| matches!(block, ContentBlock::ToolResult(tool) if tool.parts().iter()
            .any(|part| matches!(part, ToolResultPart::Text(text) if text.text().contains("omitted"))))));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn byte_measured_read_clip_pressure_failure_allows_a_fitting_next_dispatch() -> TestResult {
    let (_directory, agent, model) =
        byte_measured_read_fixture(LocalDurabilityDisposition::DurableRequired, true).await?;
    model.refuse_compaction_prepare();
    let result = agent.prompt("read the large file").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(result.model_rounds(), 2);
    assert_eq!(result.tool_calls(), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn byte_measured_read_nonfitting_successor_fails_before_dispatch() -> TestResult {
    let (_directory, agent, model) =
        byte_measured_read_fixture(LocalDurabilityDisposition::DurableRequired, true).await?;
    model.refuse_compaction_prepare();
    model
        .tool_successor_capacity_loss
        .store(40_000, Ordering::Release);
    let result = agent.prompt("read the large file").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Failed(error) if error.model_kind() == Some(ModelErrorKind::ContextLimit))
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    assert_eq!(result.model_rounds(), 1);
    assert_eq!(result.tool_calls(), 1);
    agent.shutdown().await?;
    Ok(())
}
