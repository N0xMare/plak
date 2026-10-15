use super::*;

/// A descriptor whose context window is small enough to reach in a test.
///
/// One shared window supplies the headroom fence and, under the 128-token
/// output reservation, the admitted input ceiling, so a single number drives
/// both the fence and the dispatch gate.
fn placeholder_test_context_contract(
    protocol: ProtocolId,
    observed_at: Timestamp,
    ceiling: u64,
) -> TestResult<ContextContract> {
    placeholder_test_context_contract_rule(
        protocol,
        observed_at,
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(ceiling),
        },
    )
}

fn placeholder_test_context_contract_rule(
    protocol: ProtocolId,
    observed_at: Timestamp,
    rule: CapacityRule,
) -> TestResult<ContextContract> {
    let freshness = EvidenceFreshness::new(observed_at, None)?;
    Ok(ContextContract::new(
        protocol,
        "artifact-accounting-v1",
        "artifact-projection-v1",
        "artifact-reservation-v1",
        CapacityCertificate::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "agent-scripted-capacity",
            "agent-scripted-capacity-v1",
            ContextEvidenceSource::BuiltInCatalog,
            rule,
            None,
            freshness,
        )?,
        vec![MeasurementCertificate::standard(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "agent-scripted-counter",
            "agent-scripted-exact-v1",
            "agent-scripted-complete-projection",
            "agent-scripted-method-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CountCertificateKind::ExactTokenizer,
            None,
            freshness,
        )?],
    )?)
}

pub(super) fn placeholder_test_descriptor(
    ceiling: u64,
    facet: bool,
) -> TestResult<ResolvedModelDescriptor> {
    let observed_at = Timestamp::new(10, 0)?;
    let descriptor = if facet {
        native_compact_test_descriptor_with_facet("test.route", observed_at)?
    } else {
        durable_test_descriptor_at("test.route", observed_at)?
    };
    let protocol = descriptor.protocol().clone();
    Ok(
        descriptor.with_context_contract(placeholder_test_context_contract(
            protocol,
            observed_at,
            ceiling,
        )?),
    )
}

/// A read-only `probe` whose successive outcomes carry the given body sizes.
pub(super) fn oversized_tool_registry(bodies: &[usize]) -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Probe one value.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::model_content(128, 1_048_576)?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let outcomes = bodies
        .iter()
        .map(|bytes| ToolOutcome::success_text("x".repeat(*bytes)))
        .collect::<Vec<_>>();
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        Arc::new(plakous_test_support::ScriptedTool::new(outcomes)),
    ))?;
    Ok(registry.freeze())
}

/// One turn whose tool rounds return bodies large enough to cross the fence.
/// [`placeholder_fixture`] whose compaction request cannot be prepared.
pub(super) fn unpreparable_compaction_fixture(
    workspace: Arc<WorkspacePolicy>,
    entry_base: u128,
    ceiling: u64,
    bodies: &[usize],
    tool_rounds: &[usize],
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    let (builder, model) = placeholder_fixture(
        workspace,
        entry_base,
        ceiling,
        bodies,
        tool_rounds,
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.refuse_compaction_prepare();
    Ok((builder, model))
}

pub(super) fn placeholder_fixture(
    workspace: Arc<WorkspacePolicy>,
    entry_base: u128,
    ceiling: u64,
    bodies: &[usize],
    tool_rounds: &[usize],
    facet: bool,
    durability: LocalDurabilityDisposition,
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    placeholder_fixture_producing_artifacts(
        workspace,
        entry_base,
        ceiling,
        bodies,
        tool_rounds,
        facet,
        durability,
        0,
    )
}

/// [`placeholder_fixture`] whose first `candidate_calls` responses publish an
/// active family artifact, so what a later call attaches is observable.
#[allow(
    clippy::too_many_arguments,
    reason = "one test fixture constructor, every axis explicit at the call site"
)]
pub(super) fn placeholder_fixture_producing_artifacts(
    workspace: Arc<WorkspacePolicy>,
    entry_base: u128,
    ceiling: u64,
    bodies: &[usize],
    tool_rounds: &[usize],
    facet: bool,
    durability: LocalDurabilityDisposition,
    candidate_calls: usize,
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    placeholder_fixture_pausing(
        workspace,
        entry_base,
        ceiling,
        bodies,
        tool_rounds,
        facet,
        durability,
        candidate_calls,
        &[],
    )
}

/// [`placeholder_fixture`] whose model server-pauses on the named calls.
#[allow(
    clippy::too_many_arguments,
    reason = "one test fixture constructor, every axis explicit at the call site"
)]
pub(super) fn placeholder_fixture_pausing(
    workspace: Arc<WorkspacePolicy>,
    entry_base: u128,
    ceiling: u64,
    bodies: &[usize],
    tool_rounds: &[usize],
    facet: bool,
    durability: LocalDurabilityDisposition,
    candidate_calls: usize,
    pauses: &[usize],
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    placeholder_fixture_with_descriptor(
        workspace,
        entry_base,
        placeholder_test_descriptor(ceiling, facet)?,
        bodies,
        tool_rounds,
        durability,
        candidate_calls,
        pauses,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the scripted fixture keeps every authority axis explicit"
)]
pub(super) fn placeholder_fixture_with_descriptor(
    workspace: Arc<WorkspacePolicy>,
    entry_base: u128,
    descriptor: ResolvedModelDescriptor,
    bodies: &[usize],
    tool_rounds: &[usize],
    durability: LocalDurabilityDisposition,
    candidate_calls: usize,
    pauses: &[usize],
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            candidate_calls,
            entry_base,
            false,
        )
        .with_provider_pauses(pauses.iter().copied())
        .with_scripted_tool_calls(
            tool_rounds
                .iter()
                .map(|round| (*round, "probe", "{}".to_owned())),
        )
        .with_byte_derived_context_tokens(),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = oversized_tool_registry(bodies)?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        durability,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .with_scheduled_compaction_enabled(true);
    Ok((builder, model))
}

#[tokio::test]
async fn offline_pass_on_artifact_gated_replay_does_not_withhold_every_following_batch()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let mut bodies = vec![20_000, 180_000];
    bodies.extend([200; 10]);
    let rounds = (0..12).collect::<Vec<_>>();
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace,
        110_000,
        150_000,
        &bodies,
        &rounds,
        true,
        LocalDurabilityDisposition::Ephemeral,
        30,
    )?;
    model
        .artifact_gated_reasoning_bytes
        .store(40_000, Ordering::Release);
    model.item_context.store(true, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(12_000, Ordering::Release);
    model.refuse_compaction_prepare();
    let agent = builder
        .config(
            AgentConfig::default()
                .with_event_capacity(8192)
                .with_event_byte_capacity(64 * 1024 * 1024),
        )
        .with_scheduled_compaction_enabled(true)
        .build()?;
    let mut turn = agent
        .start(CommandId::new("gated:long-turn")?, "read the files")
        .await?
        .into_turn()
        .ok_or("turn not admitted")?;
    let mut events = turn.take_events()?;
    let mut passes = 0;
    let mut withheld_after = 0;
    loop {
        match events.recv().await?.kind() {
            AgentEventKind::ToolResultsReplaced { .. } => passes += 1,
            AgentEventKind::ObservationReplaced { .. } if passes > 0 => withheld_after += 1,
            AgentEventKind::TurnCompleted { .. } => break,
            AgentEventKind::TurnFailed { .. } => {
                return Err("artifact-gated turn exhausted context".into());
            }
            _ => {}
        }
    }
    assert_eq!(passes, 1, "one offline pass must end the compaction loop");
    assert_eq!(
        withheld_after, 0,
        "historical replay must not be resurrected"
    );
    let checkpoint = agent.checkpoint();
    let history = checkpoint.manifest().canonical().history();
    assert!(history.segments().iter().flat_map(HistorySegment::items).flat_map(ConversationItem::blocks)
        .any(|b| matches!(b, ContentBlock::Text(t) if t.text().contains("earlier reasoning dropped to fit"))));
    let items = history
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .collect::<Vec<_>>();
    let replay_note = items.iter().position(|item| item.blocks().iter().any(
        |block| matches!(block, ContentBlock::Text(text) if text.text().contains("earlier reasoning dropped to fit"))
    )).ok_or("replay-loss note")?;
    assert!(
        items[..replay_note]
            .iter()
            .flat_map(|item| item.blocks())
            .all(|block| !matches!(block, ContentBlock::Reasoning(_)))
    );
    // The retained reasoning was produced after the pass, never copied through it.
    let reasoning = history
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter(|b| matches!(b, ContentBlock::Reasoning(_)))
        .count();
    assert!(
        reasoning < 12,
        "the offline successor must delete historical reasoning"
    );
    {
        let measured = lock_mutex(&model.gated_fresh_wires);
        let (history, wire, _) = measured
            .iter()
            .find(|(history, _, live)| {
                *live
                    && measured
                        .iter()
                        .any(|(candidate, _, live)| !live && candidate == history)
            })
            .ok_or("published successor was not prepared as the next live input")?;
        let (_, probed_wire, _) = measured
            .iter()
            .find(|(candidate, _, live)| !live && candidate == history)
            .ok_or("successor fit probe")?;
        assert_eq!(
            wire, probed_wire,
            "published successor must lower to the measured wire"
        );
    }
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn artifact_gated_long_turn_soak_bounds_compactions_and_eventually_dispatches_paid()
-> TestResult {
    const ROUNDS: usize = 64;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let mut bodies = vec![20_000, 120_000];
    bodies.extend(vec![2_000; ROUNDS - 2]);
    let rounds = (0..ROUNDS).collect::<Vec<_>>();
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace,
        114_000,
        100_000,
        &bodies,
        &rounds,
        true,
        LocalDurabilityDisposition::Ephemeral,
        ROUNDS + 10,
    )?;
    model
        .artifact_gated_reasoning_bytes
        .store(40_000, Ordering::Release);
    model.item_context.store(true, Ordering::Release);
    model.script_tool_rounds_around_compactions();
    model.refuse_native_until_round.store(3, Ordering::Release);
    let agent = builder
        .config(
            AgentConfig::default()
                .with_max_model_rounds(256)
                .with_event_capacity(8192)
                .with_event_byte_capacity(64 * 1024 * 1024),
        )
        .with_scheduled_compaction_enabled(true)
        .build()?;
    let mut turn = agent
        .start(CommandId::new("gated:soak")?, "read all files")
        .await?
        .into_turn()
        .ok_or("turn not admitted")?;
    let mut events = turn.take_events()?;
    let mut compactions = 0_u64;
    loop {
        match events.recv().await?.kind() {
            AgentEventKind::CompactionFinished {
                outcome: CompactionEventOutcome::Committed,
                ..
            } => compactions += 1,
            AgentEventKind::TurnCompleted { .. } => break,
            AgentEventKind::TurnFailed { .. } => {
                return Err("long-turn soak exhausted context".into());
            }
            _ => {}
        }
    }
    let growth = (ROUNDS as u64 * 40_000 + bodies.iter().sum::<usize>() as u64).div_ceil(4);
    assert!(
        compactions <= growth.div_ceil(50_000) + 1,
        "{compactions} compactions for {growth} tokens of growth"
    );
    assert!(model.paid_compaction_calls.load(Ordering::Acquire) >= 1);
    assert_eq!(model.ordinary_calls.load(Ordering::Acquire), ROUNDS + 1);
    agent.shutdown().await?;
    Ok(())
}

async fn pressure_guard_regression(refuse_first: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let bodies = if refuse_first {
        vec![20_000, 60_000, 60_000, 2_000]
    } else {
        vec![20_000, 60_000, 60_000, 60_000, 60_000, 2_000]
    };
    let rounds = (0..bodies.len()).collect::<Vec<_>>();
    let descriptor = placeholder_test_descriptor(40_000, true)?;
    let (builder, model) = placeholder_fixture_with_descriptor(
        workspace,
        111_000,
        descriptor,
        &bodies,
        &rounds,
        LocalDurabilityDisposition::Ephemeral,
        0,
        &[],
    )?;
    model.script_tool_rounds_around_compactions();
    if refuse_first {
        model.refuse_native_until_round.store(3, Ordering::Release);
    } else {
        model.item_context.store(true, Ordering::Release);
        model
            .item_provider_input_tokens
            .store(36_000, Ordering::Release);
    }
    let agent = builder.build()?;
    let mut turn = agent
        .start(CommandId::new("pressure:guard")?, "read the files")
        .await?
        .into_turn()
        .ok_or("turn not admitted")?;
    let mut events = turn.take_events()?;
    let mut refusal_detail = false;
    loop {
        match events.recv().await?.kind() {
            AgentEventKind::CompactionFinished {
                outcome: CompactionEventOutcome::Committed,
                detail,
                ..
            } => {
                refusal_detail |= detail
                    .as_ref()
                    .is_some_and(|d| d.contains("first pressure native input"));
            }
            AgentEventKind::TurnCompleted { .. } => break,
            AgentEventKind::TurnFailed { .. } => {
                return Err(format!(
                    "pressure turn exhausted context; paid={}, ordinary={}, history={:?}",
                    model.paid_compaction_calls.load(Ordering::Acquire),
                    model.ordinary_calls.load(Ordering::Acquire),
                    agent.checkpoint().manifest().canonical().history()
                )
                .into());
            }
            _ => {}
        }
    }
    if refuse_first {
        assert!(
            model.paid_compaction_calls.load(Ordering::Acquire) >= 1,
            "an unadmitted first pressure fence must not spend the paid guard"
        );
        assert!(
            refusal_detail,
            "the committed offline rung must explain native preparation refusal"
        );
    } else {
        assert!(
            model.paid_compaction_calls.load(Ordering::Acquire) >= 2,
            "spent pressure guard must not mask anchored occupancy"
        );
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn unadmitted_native_at_first_pressure_fence_does_not_spend_the_pressure_guard() -> TestResult
{
    pressure_guard_regression(true).await
}

#[tokio::test]
async fn spent_pressure_guard_still_allows_an_occupancy_triggered_paid_fold() -> TestResult {
    pressure_guard_regression(false).await
}

#[tokio::test]
async fn placeholder_pass_rejects_successor_that_grows_the_fresh_lowering() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace.clone(),
        112_000,
        100_000,
        &[100, 35],
        &[0, 1],
        true,
        LocalDurabilityDisposition::Ephemeral,
        10,
    )?;
    model
        .artifact_gated_reasoning_bytes
        .store(40_000, Ordering::Release);
    model.item_context.store(true, Ordering::Release);
    let agent = builder.with_scheduled_compaction_enabled(false).build()?;
    assert!(matches!(
        agent
            .prompt("read small results")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let before = agent.checkpoint();
    let topology = before.manifest().topology();
    let (builder, compact_model) = placeholder_fixture(
        workspace,
        113_000,
        100_000,
        &[],
        &[],
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    compact_model
        .artifact_gated_reasoning_bytes
        .store(40_000, Ordering::Release);
    compact_model.item_context.store(true, Ordering::Release);
    compact_model.refuse_compaction_prepare();
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:fresh-growth".to_owned(),
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await;
    assert!(
        !matches!(outcome, Ok(AgentCompactSessionOutcome::Compacted { .. })),
        "fresh wire grew, but the artifact-inflated source accepted it: {outcome:?}"
    );
    assert_eq!(
        agent.checkpoint().manifest().canonical().history(),
        before.manifest().canonical().history()
    );
    agent.shutdown().await?;
    Ok(())
}

/// A window the fully replaced head cannot fit, but the opening intent can.
///
/// The irreducible remainder — the stable prefix, the user intent, the tool
/// call, the placeholder and the notice — is what exceeds this, so the ladder
/// is genuinely exhausted rather than refused before it starts.
const PLACEHOLDER_UNREACHABLE_CEILING: u64 = 700;

/// Every placeholders harness note the history carries.
pub(super) fn placeholder_harness_notes(history: &CanonicalHistory) -> Vec<String> {
    history
        .segments()
        .iter()
        .filter(|segment| segment.kind() == HistorySegmentKind::Steering)
        .flat_map(|segment| segment.items().iter())
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text)
                if text.text().starts_with(
                    crate::runtime::compaction::placeholder::PLACEHOLDER_HARNESS_NOTE_MARKER,
                ) =>
            {
                Some(text.text().to_owned())
            }
            _ => None,
        })
        .collect()
}

/// Every replaced placeholder in a canonical history, oldest first.
pub(super) fn placeholder_bodies(history: &CanonicalHistory) -> Vec<String> {
    let mut found = Vec::new();
    for segment in history.segments() {
        for item in segment.items() {
            for block in item.blocks() {
                if let ContentBlock::ToolResult(result) = block {
                    for part in result.parts() {
                        if let ToolResultPart::Text(text) = part
                            && text.text().starts_with(
                                crate::runtime::compaction::placeholder::REPLACED_TOOL_RESULT_MARKER,
                            )
                        {
                            found.push(text.text().to_owned());
                        }
                    }
                }
            }
        }
    }
    found
}

pub(super) fn compaction_segment_count(history: &CanonicalHistory) -> usize {
    history
        .segments()
        .iter()
        .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
        .count()
}

/// The newest tool batch remains available for the model to consume.
///
/// Admission may clip an oversized observation, but proactive compaction must
/// not replace the newest result and induce a reread loop. Once a later round
/// arrives, the older sealed middle can be folded while the fresh result
/// remains intact for the next model call.
#[tokio::test]
async fn proactive_compaction_retains_the_fresh_tool_batch() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        71_000,
        70_000,
        &[25_000],
        &[1],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.item_context.store(true, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(40_000, Ordering::Release);
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    assert!(matches!(
        agent
            .prompt("older intent ".repeat(150))
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let result = agent.prompt("read the big file").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "the turn must answer instead of refusing: {:?}",
        result.outcome()
    );
    let checkpoint = agent.checkpoint();
    let manifest = checkpoint.manifest();
    let history = manifest.canonical().history();
    let placeholders = placeholder_bodies(history);
    assert!(
        placeholders.is_empty(),
        "the paid fold wins before the placeholder rung"
    );
    assert!(
        history
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::ToolExchange)
            .fold(None, |_, segment| Some(segment))
            .into_iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .flat_map(ToolResultBlock::parts)
            .any(
                |part| matches!(part, ToolResultPart::Text(text) if !text.text().starts_with(
                    crate::runtime::compaction::placeholder::REPLACED_TOOL_RESULT_MARKER
                ))
            ),
        "the fresh result remains available rather than becoming a compaction placeholder"
    );
    assert_eq!(
        compaction_segment_count(history),
        1,
        "the older sealed middle is folded once"
    );
    assert!(
        manifest
            .canonical()
            .context()
            .compaction_provenance()
            .is_some_and(|provenance| provenance.strategy()
                == RuntimeCompactionStrategy::SemanticSummaryPrefixV1)
    );
    let calls = history
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
        .count();
    assert_eq!(
        calls, 1,
        "the protected fresh tool exchange survives the fold"
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        4,
        "one older round, one tool round, one paid fold and one answering round"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A route that does have the facet never reaches the placeholder rung.
#[tokio::test]
async fn the_native_facet_still_wins_over_the_placeholder_rung() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, _) = placeholder_fixture(
        workspace,
        72_000,
        40_000,
        &[72_000, 2_000],
        &[0, 1],
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let result = agent.prompt("read the big file").await?.result().await?;
    assert!(matches!(
        result.outcome(),
        TurnResultOutcome::Completed(_) | TurnResultOutcome::Failed(_)
    ));
    let checkpoint = agent.checkpoint();
    let manifest = checkpoint.manifest();
    assert!(
        placeholder_bodies(manifest.canonical().history()).is_empty(),
        "the native facet compacts this route; the placeholder rung must not run"
    );
    agent.shutdown().await?;
    Ok(())
}

/// The ladder runs out without discarding the fresh result.
///
/// This is the reachable shape of "a result larger than the window". The
/// ceiling is above the stable prefix and the opening intent, so the session
/// opens and the first round dispatches; after admission clipping, the fresh
/// result is still the irreducible protected tail. The existing typed overflow
/// refusal survives without a reread or a paid dispatch.
#[tokio::test]
async fn the_refusal_survives_when_even_full_placeholders_cannot_fit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        73_000,
        PLACEHOLDER_UNREACHABLE_CEILING,
        &[72_000, 2_000],
        &[0, 1],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let mut turn = agent.prompt("read the big file").await?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Failed(_)),
        "a head nothing can fit still refuses: {:?}",
        result.outcome()
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "the admitted opening round is not reread after the irreducible overflow"
    );
    assert!(
        model.observed_compaction_contexts().is_empty(),
        "protected-only overflow dispatches no paid compaction"
    );
    assert_eq!(
        result.tool_calls(),
        1,
        "the tool round ran before the fence"
    );
    let capacity = result
        .context_capacity()
        .ok_or("an exhausted ladder must still report its typed capacity boundary")?;
    assert!(capacity.failure().overflow().shared().is_some());
    assert!(matches!(
        result.context_overflow_remedy(),
        Some(crate::ContextOverflowRemedy::Compact | crate::ContextOverflowRemedy::ClearOrFork)
    ));
    let checkpoint = agent.checkpoint();
    let manifest = checkpoint.manifest();
    assert!(
        manifest
            .canonical()
            .context()
            .compaction_provenance()
            .is_none(),
        "a refused ladder publishes no compaction"
    );
    assert_eq!(
        compaction_segment_count(manifest.canonical().history()),
        0,
        "a refused ladder publishes no notice either"
    );
    let mut terminal_events = 0;
    loop {
        let event = events.recv().await?;
        match event.kind() {
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => {
                terminal_events += 1;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        terminal_events, 1,
        "the refused root has one terminal result"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A small prompt on a history that itself overflows is not the cause, so it
/// stays in history behind the failure note and the remedy is about history.
#[tokio::test]
async fn a_small_prompt_on_an_overflowing_history_is_kept_behind_a_note() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, _model) = placeholder_fixture(
        workspace,
        73_700,
        PLACEHOLDER_UNREACHABLE_CEILING,
        &[72_000, 2_000],
        &[0, 1],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let first = agent.prompt("read the big file").await?.result().await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Failed(_)));

    let follow_up = "a short follow-up";
    let failed = agent.prompt(follow_up).await?.result().await?;
    assert!(
        matches!(failed.outcome(), TurnResultOutcome::Failed(_)),
        "{:?}",
        failed.outcome()
    );
    let remedy = failed.context_overflow_remedy();
    assert!(
        matches!(
            remedy,
            Some(crate::ContextOverflowRemedy::Compact | crate::ContextOverflowRemedy::ClearOrFork)
        ),
        "{remedy:?}"
    );
    let items = failed.conversation().items().to_vec();
    let [.., prompt, note] = items.as_slice() else {
        return Err("failed turn history is too short".into());
    };
    assert_eq!(text_of(prompt), follow_up);
    assert_eq!(prompt.provenance(), Provenance::UserInput);
    assert_eq!(note.actor(), Actor::User);
    assert_eq!(note.provenance(), Provenance::Instruction);
    assert!(text_of(note).starts_with("the turn handling the request above failed (ContextLimit)"));
    agent.shutdown().await?;
    Ok(())
}

/// A prompt larger than the window is withheld, not replayed.
///
/// The history without it fits, so the prompt is the cause. Kept verbatim it
/// would be part of every later request, and of the fold's own input, so every
/// following turn would refuse the same way. The failed turn
/// names the remedy, its prompt is replaced by a note, and the next prompt runs.
#[tokio::test]
async fn an_oversized_prompt_is_withheld_and_the_next_turn_runs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        73_500,
        PLACEHOLDER_UNREACHABLE_CEILING,
        &[100],
        &[],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let oversized = "x".repeat(16_000);
    let failed = agent.prompt(oversized.clone()).await?.result().await?;
    assert!(
        matches!(failed.outcome(), TurnResultOutcome::Failed(_)),
        "{:?}",
        failed.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    assert_eq!(
        failed.context_overflow_remedy(),
        Some(crate::ContextOverflowRemedy::ResendShorter)
    );
    let items = failed.conversation().items().to_vec();
    assert!(items.iter().all(|item| text_of(item) != oversized));
    let note = items.last().ok_or("withheld prompt note missing")?;
    assert_eq!(note.actor(), Actor::User);
    assert_eq!(note.provenance(), Provenance::Instruction);
    assert!(text_of(note).contains("user message of 16000 bytes was withheld"));

    let next = agent.prompt("a short question").await?.result().await?;
    assert!(
        matches!(next.outcome(), TurnResultOutcome::Completed(_)),
        "the withheld prompt must not refuse the next turn: {:?}",
        next.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

/// A durable session reopened after a placeholder pass restores exactly.
#[tokio::test]
async fn a_durable_resume_across_placeholders_restores_the_replaced_head() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (builder, model) = placeholder_fixture(
        Arc::clone(&workspace),
        74_000,
        100_000,
        &[60_000, 60_000, 60_000, 60_000, 2_000],
        &[0, 1, 2, 3, 4],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.fail_compaction_call();
    let agent = sessions
        .create("create:placeholder-resume", builder)
        .await?;
    let result = agent.prompt("read the big file").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let history_before = agent.checkpoint().manifest().canonical().history().clone();
    let provenance_before = agent
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("placeholder provenance")?
        .identity();
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let (reopen_builder, _) = placeholder_fixture(
        workspace,
        74_500,
        100_000,
        &[60_000, 60_000, 60_000, 60_000, 2_000],
        &[0, 1, 2, 3, 4],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:placeholder-resume", address, reopen_builder)
        .await?
    else {
        return Err("an replaced quiescent head must reopen without recovery".into());
    };
    let reopened_checkpoint = reopened.checkpoint();
    let manifest = reopened_checkpoint.manifest();
    assert_eq!(
        manifest.canonical().history().identity(),
        history_before.identity(),
        "the replaced successor is restored byte-exactly"
    );
    assert_eq!(
        manifest
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("restored placeholder provenance")?
            .identity(),
        provenance_before
    );
    assert_eq!(placeholder_bodies(manifest.canonical().history()).len(), 2);
    reopened.shutdown().await?;
    Ok(())
}

/// With an older intent group to fold, the ladder never reaches placeholders.
///
/// Rung two is the semantic summary; it replaces whole eligible intent groups
/// and leaves the protected tail's tool bodies alone.
#[tokio::test]
async fn the_fold_still_wins_when_an_older_intent_group_is_eligible() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, _) = placeholder_fixture(
        workspace,
        75_000,
        40_000,
        // One tool round in the second turn: the newest batch is never
        // replaceable, so the ladder reaching rung two is unambiguous.
        &[72_000],
        &[1],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let first = agent
        .prompt("older replaceable intent")
        .await?
        .result()
        .await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
    let second = agent.prompt("read the big file").await?.result().await?;
    assert!(matches!(second.outcome(), TurnResultOutcome::Completed(_)));
    let checkpoint = agent.checkpoint();
    let manifest = checkpoint.manifest();
    let history = manifest.canonical().history();
    assert!(
        placeholder_bodies(history).is_empty(),
        "the fold was available; the placeholder rung must not run"
    );
    let first_segment = history.segments().get(0).ok_or("a folded successor")?;
    assert_eq!(
        first_segment.kind(),
        HistorySegmentKind::Compaction,
        "the semantic fold puts its summary first"
    );
    let provenance = manifest
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("fold provenance")?;
    assert_eq!(
        provenance.summary_segment_identity(),
        first_segment.identity()
    );
    agent.shutdown().await?;
    Ok(())
}

/// The compaction prepares fresh: no artifact, no response continuation.
///
/// A compaction's input is the sealed cut, a strict subset of the history the
/// live turn's response and its active family artifact cover. Attaching either
/// asks the route to reuse state that does not match this request's canonical
/// coverage. On the Responses wire `from_active_artifact` refuses that outright
/// on a coverage mismatch, which is how the live Grok and Codex growth turns of
/// 2026-09-18 lost every compaction they attempted. The previous-response
/// continuation is a second, separate attachment with only Messages readers; it
/// is pinned here too so neither can be re-attached by a future caller.
#[tokio::test]
async fn the_prepared_compaction_attaches_no_artifact_and_no_continuation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace,
        75_000,
        40_000,
        &[72_000],
        &[1],
        false,
        LocalDurabilityDisposition::Ephemeral,
        // Every ordinary call publishes an artifact, so a compaction that
        // attached one would be caught.
        16,
    )?;
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let first = agent
        .prompt("older replaceable intent")
        .await?
        .result()
        .await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
    let second = agent.prompt("read the big file").await?.result().await?;
    assert!(matches!(second.outcome(), TurnResultOutcome::Completed(_)));

    let compaction_contexts = model.observed_compaction_contexts();
    assert!(
        !compaction_contexts.is_empty(),
        "this fixture must actually fold, or the pin asserts nothing"
    );
    for context in &compaction_contexts {
        assert!(
            context.active_family_artifact().is_none(),
            "a compaction must prepare fresh, never against the live turn's artifact"
        );
        assert!(
            context.previous_response_metadata().is_none(),
            "a compaction must not continue the live turn's response"
        );
    }
    // The fixture does attach an artifact on ordinary calls, so the assertions
    // above discriminate the compaction rather than restating a fixture that
    // never attaches anything.
    assert!(
        model.observed().iter().any(Option::is_some),
        "the fixture must attach an active artifact somewhere"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A clipped tool observation reaches a turn subscriber.
///
/// 2026-09-18: offline-e2e could not make this fire from a real process at any
/// ceiling from 4,200 to 80,000 bytes and had to leave two end-to-end cases
/// ignored. This is the in-process form of those cases, and it passes, which
/// localises the remaining failure rather than reproducing it: with the budget
/// capped, the clip is computed, published with its call context, and delivered
/// to a turn subscriber.
///
/// **Root cause on the durable path.** A durable session
/// does not canonicalize its tool results at the driver sites this test
/// exercises. It canonicalizes them in `reconcile_hosted_tool_slots`
/// (`continuation.rs:638`), from the journaled completed-tool evidence, using
/// the plain `canonicalize_tool_execution` — the form that *discards* the clip.
/// So `clip_tool_result` runs and computes the clip, and
/// `publish_observation_clip` is never called at all: a probe there prints
/// nothing on a real `plak agent` run, not even its "no clip" branch, while
/// offline-e2e's probe inside `clip_tool_result` prints `budget=Some(2048)` on
/// the same turn.
///
/// That is my own design decision misapplied. I kept the plain form for "the
/// restore path, so a reopen does not re-announce a truncation the operator was
/// already told about". But this is not only the crash-reopen path: it is the
/// ordinary durable tool round, which every hosted session takes. The clip is
/// computed and thrown away on the only path the product runs.
///
/// The fix is to surface the clips from `reconcile_hosted_tool_slots` and
/// publish them in `resume_hosted_tool_batch`, which holds
/// `command.shared.events`. `driver/composition.rs:1649` and `:1996` still use
/// the plain form too and want the same treatment.
///
/// This also explains the shape asymmetry offline-e2e found without it being
/// the cause: `ObservationClipped` is call-scoped and its siblings are
/// turn-scoped, but the validator never sees the event, because it is never
/// published.
///
/// **Earlier measurement, superseded but still true.** Instrumenting a real
/// `plak agent` run over offline-e2e's durable home, with the budget capped to
/// 512 bytes and a tool returning 60,000:
///
/// - last known good: `clip_tool_result` runs with `budget=Some(512)`, so the
///   budget is **not** `None` on the durable hosted path and the clip is
///   computed there;
/// - first known bad: the hosted turn drive's event stream receives
///   `tool_completed` and then `model_started`, with no `observation_clipped`
///   between them.
///
/// An in-process turn subscription does receive it, which this test proves, so
/// the difference is the subscription the hosted drive consumes rather than the
/// publication. That is the next thing to look at, and it is where offline-e2e's
/// two ignored cases and this one meet.
///
/// Note the durability disposition. This builds `Ephemeral`, and the hosted
/// product runs `DurableRequired`, which journals an effect-decision frame
/// between the model round and the tool round. The budget is restored from that
/// frame in `driver/turn.rs`, so the durable path has a boundary this one does
/// not cross. A companion covering the durable path belongs beside this.
#[tokio::test]
async fn a_clipped_tool_observation_publishes_what_it_cut() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, _) = placeholder_fixture(
        workspace,
        76_000,
        400_000,
        // One tool round returning far more than the capped budget allows.
        &[72_000],
        &[0],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder
        .config(AgentConfig::default().with_observation_budget_cap_bytes(Some(2_048)))
        .build()
        .map_err(|error| error.message().to_owned())?;
    let mut turn = agent
        .start(CommandId::new("clip:observation")?, "read the big file")
        .await?
        .into_turn()
        .ok_or("the clip turn was not admitted")?;
    let mut events = turn.take_events()?;
    let mut clip = None;
    loop {
        let event = events.recv().await?;
        match event.kind() {
            AgentEventKind::ObservationClipped {
                name,
                bytes_kept,
                bytes_dropped,
                bound,
            } => {
                // The context is the call, so a surface can attach the note to
                // the tool row whose output was cut.
                assert!(event.turn_id().is_some(), "the clip names its turn");
                assert!(event.call_id().is_some(), "the clip names its call");
                clip = Some((name.to_string(), *bytes_kept, *bytes_dropped, *bound));
            }
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    let (name, kept, dropped, bound) =
        clip.ok_or("a tool result over the budget must publish what it cut")?;
    assert_eq!(name, "probe");
    assert!(
        bound <= 2_048,
        "the cap can only shrink the budget: {bound}"
    );
    assert!(dropped > 0, "a clipped result dropped something");
    assert_eq!(
        kept.saturating_add(dropped),
        72_000,
        "kept plus dropped is the original body, so neither figure double counts"
    );
    agent.shutdown().await?;
    Ok(())
}
