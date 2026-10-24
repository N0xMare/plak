use std::io;

use super::*;
use crate::{
    Actor, AttemptId, ContentBlock, ConversationItem, EntryId, HistorySegment, HistorySegmentKind,
    Provenance, RunId, SemanticJsonV1, TextBlock, Timestamp, TurnId, WorkingProjection,
};
use uuid::Uuid;

#[test]
fn semantic_json_sorts_object_keys_but_preserves_arrays() -> Result<(), RequestError> {
    let first = serde_json::json!({"b": [2, 1], "a": true});
    let second = serde_json::json!({"a": true, "b": [2, 1]});
    let first = SemanticJsonV1::from_value(first)?;
    let second = SemanticJsonV1::from_value(second)?;
    assert_eq!(first.canonical_bytes(), second.canonical_bytes());
    assert_eq!(first.canonical_bytes().as_ref(), br#"{"a":true,"b":[2,1]}"#);
    Ok(())
}

#[test]
fn native_option_numbers_take_their_identity_spelling() -> Result<(), Box<dyn std::error::Error>> {
    let namespace = NamespaceId::new("plakous.test")?;
    let options = |value: Value| NativeOptions::new(BTreeMap::from([(namespace.clone(), value)]));
    let spelled = options(serde_json::json!({"t": 1.0, "n": [1e2, 0.5], "s": "1.0"}));
    let canonical = options(serde_json::json!({"t": 1, "n": [100, 0.5], "s": "1.0"}));
    assert_eq!(spelled, canonical);
    assert_eq!(
        serde_json::to_string(&spelled)?,
        r#"{"entries":{"plakous.test":{"n":[100,0.5],"s":"1.0","t":1}}}"#
    );
    let decoded: NativeOptions =
        serde_json::from_str(r#"{"entries":{"plakous.test":{"t":1.0,"n":[1e2,0.5],"s":"1.0"}}}"#)?;
    assert_eq!(decoded, canonical);
    assert_ne!(
        options(serde_json::json!({"t": 1.5})),
        options(serde_json::json!({"t": 1}))
    );
    Ok(())
}

#[test]
fn invalid_schema_is_rejected() {
    let result = ToolInputContract::structured(serde_json::json!({"type": "unsupported"}));
    assert!(result.is_err());
}

#[test]
fn model_content_bounds_are_validated_serialized_and_identity_bearing()
-> Result<(), Box<dyn std::error::Error>> {
    for invalid in [
        serde_json::json!({"max_parts": 0, "max_bytes": 1}),
        serde_json::json!({"max_parts": 129, "max_bytes": 1}),
        serde_json::json!({"max_parts": 1, "max_bytes": 0}),
        serde_json::json!({"max_parts": 1, "max_bytes": 1_048_577}),
        serde_json::json!({"max_parts": 1, "max_bytes": 1, "unknown": true}),
    ] {
        assert!(serde_json::from_value::<ModelContentContractV1>(invalid).is_err());
    }
    let contract = ModelContentContractV1::new(2, 64)?;
    assert_eq!(
        serde_json::from_value::<ModelContentContractV1>(serde_json::to_value(contract)?)?,
        contract
    );

    let definition = |max_parts, max_bytes| -> Result<_, Box<dyn std::error::Error>> {
        Ok(ToolDefinition::new(
            ToolName::new("lookup")?,
            "1.0.0",
            "Looks up a value",
            ToolInputContract::structured(empty_object_schema())?,
            ToolOutputContract::model_content(max_parts, max_bytes)?,
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )?)
    };
    let baseline = definition(2, 64)?;
    assert_eq!(baseline.digest().version(), 4);
    assert_ne!(baseline.digest(), definition(1, 64)?.digest());
    assert_ne!(baseline.digest(), definition(2, 63)?.digest());
    assert_eq!(
        serde_json::from_value::<ToolDefinition>(serde_json::to_value(&baseline)?)?,
        baseline
    );
    Ok(())
}

#[test]
fn resource_concurrency_is_canonical_and_rejects_ambiguous_keys()
-> Result<(), Box<dyn std::error::Error>> {
    let first = ToolDefinition::new(
        ToolName::new("lookup")?,
        "1.0.0",
        "Looks up a value",
        ToolInputContract::structured(empty_object_schema())?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::ResourceExclusive(vec![
            ToolResourceKey::new("workspace:b")?,
            ToolResourceKey::new("workspace:a")?,
        ]),
        ToolEffect::ReadOnly,
    )?;
    let second = ToolDefinition::new(
        ToolName::new("lookup")?,
        "1.0.0",
        "Looks up a value",
        ToolInputContract::structured(empty_object_schema())?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::ResourceExclusive(vec![
            ToolResourceKey::new("workspace:a")?,
            ToolResourceKey::new("workspace:b")?,
        ]),
        ToolEffect::ReadOnly,
    )?;

    assert_eq!(first, second);
    assert_eq!(
        first.concurrency(),
        ToolConcurrency::ResourceExclusive(vec![
            ToolResourceKey::new("workspace:a")?,
            ToolResourceKey::new("workspace:b")?,
        ])
    );
    for keys in [
        Vec::new(),
        vec![
            ToolResourceKey::new("workspace:a")?,
            ToolResourceKey::new("workspace:a")?,
        ],
    ] {
        assert!(matches!(
            ToolDefinition::new(
                ToolName::new("lookup")?,
                "1.0.0",
                "Looks up a value",
                ToolInputContract::structured(empty_object_schema())?,
                ToolOutputContract::portable_model_content(),
                ToolConcurrency::ResourceExclusive(keys),
                ToolEffect::ReadOnly,
            ),
            Err(RequestError::InvalidToolConcurrency { .. })
        ));
    }
    Ok(())
}

fn tool_definition() -> Result<ToolDefinition, Box<dyn std::error::Error>> {
    Ok(ToolDefinition::new(
        ToolName::new("lookup")?,
        "1.0.0",
        "Looks up a value",
        ToolInputContract::structured(empty_object_schema())?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Parallel,
        ToolEffect::ReadOnly,
    )?)
}

fn configured_tool() -> Result<ConfiguredTool, Box<dyn std::error::Error>> {
    Ok(ConfiguredTool::new(
        tool_definition()?,
        crate::ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    ))
}

fn empty_object_schema() -> Value {
    serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false})
}

fn metadata() -> Result<RequestMetadata, Box<dyn std::error::Error>> {
    Ok(RequestMetadata::for_turn(
        RunId::try_from(Uuid::from_u128(1))?,
        TurnId::try_from(Uuid::from_u128(2))?,
        AttemptId::try_from(Uuid::from_u128(3))?,
        Timestamp::new(1, 0)?,
    ))
}

fn request_builder(input: ConversationView, metadata: RequestMetadata) -> ModelRequestBuilder {
    let selected = SelectedProviderConfigurationId::new(
        Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"request-test-selected"],
        )
        .expect("selected configuration digest"),
    )
    .expect("selected configuration identity");
    let effective = EffectiveProviderConfigurationId::new(
        Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"request-test-effective"],
        )
        .expect("effective configuration digest"),
    )
    .expect("effective configuration identity");
    ModelRequest::builder(selected, effective, input, metadata)
}

#[test]
fn request_root_is_exact_round_tripped_and_identity_bound() -> Result<(), Box<dyn std::error::Error>>
{
    let workflow_id = SessionWorkflowRootId::new([0xa5; 32])?;
    let workflow = RequestMetadata::for_session_workflow(
        RunId::try_from(Uuid::from_u128(1))?,
        workflow_id,
        AttemptId::try_from(Uuid::from_u128(3))?,
        Timestamp::new(1, 0)?,
    );
    assert_eq!(workflow.turn_id(), None);
    assert_eq!(
        workflow.root(),
        ModelOperationRoot::SessionWorkflow(workflow_id)
    );
    assert_eq!(
        serde_json::from_slice::<RequestMetadata>(&serde_json::to_vec(&workflow)?)?,
        workflow
    );

    let turn = metadata()?;
    let turn_request = request_builder(ConversationView::default(), turn).build()?;
    let workflow_request = request_builder(ConversationView::default(), workflow).build()?;
    assert_ne!(turn_request.identity(), workflow_request.identity());
    assert_ne!(
        turn_request.canonical_bytes(),
        workflow_request.canonical_bytes()
    );
    assert_eq!(
        turn_request.metadata().turn_id(),
        Some(TurnId::try_from(Uuid::from_u128(2))?)
    );
    Ok(())
}

#[test]
fn workflow_root_rejects_zero_and_tampered_metadata() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        SessionWorkflowRootId::new([0; 32]),
        Err(crate::IdentityError::Empty)
    );
    let metadata = RequestMetadata::for_session_workflow(
        RunId::try_from(Uuid::from_u128(1))?,
        SessionWorkflowRootId::new([9; 32])?,
        AttemptId::try_from(Uuid::from_u128(3))?,
        Timestamp::new(1, 0)?,
    );
    let mut encoded = serde_json::to_value(metadata)?;
    encoded["root"]["SessionWorkflow"] = serde_json::json!(vec![0_u8; 32]);
    assert!(serde_json::from_value::<RequestMetadata>(encoded).is_err());
    assert!(!format!("{:?}", SessionWorkflowRootId::new([9; 32])?).contains('9'));
    Ok(())
}

fn user_item(value: u128, text: &str) -> Result<ConversationItem, Box<dyn std::error::Error>> {
    Ok(ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(value))?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(text, None)?)],
        Provenance::UserInput,
    )?)
}

#[test]
fn deserialization_rechecks_request_constructor_invariants()
-> Result<(), Box<dyn std::error::Error>> {
    let mut instruction =
        serde_json::to_value(InstructionBlock::new(InstructionSource::System, "valid")?)?;
    instruction["text"] = serde_json::json!("");
    assert!(serde_json::from_value::<InstructionBlock>(instruction).is_err());

    let tool = tool_definition()?;
    let mut forged_tool = serde_json::to_value(&tool)?;
    forged_tool["input_schema"] = serde_json::json!({"type": "unsupported"});
    assert!(serde_json::from_value::<ToolDefinition>(forged_tool).is_err());

    for invalid in [
        serde_json::json!({"max_bytes": 0}),
        serde_json::json!({"max_bytes": 1_048_577}),
        serde_json::json!({"max_bytes": 16, "unknown": true}),
    ] {
        assert!(serde_json::from_value::<Utf8TextContractV1>(invalid).is_err());
    }
    let utf8 = Utf8TextContractV1::new(16)?;
    assert_eq!(
        serde_json::from_value::<Utf8TextContractV1>(serde_json::to_value(utf8)?)?,
        utf8
    );

    let limits = GenerationLimits::new(None, vec![String::from("stop")])?;
    let mut invalid_limits = serde_json::to_value(limits)?;
    invalid_limits["stop_sequences"] = Value::Array(vec![serde_json::json!("x"); 17]);
    assert!(serde_json::from_value::<GenerationLimits>(invalid_limits).is_err());

    let request = request_builder(ConversationView::default(), metadata()?)
        .tools(vec![ConfiguredTool::new(
            tool,
            crate::ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectOnly,
        )])
        .build()?;
    let mut obsolete_continuation = serde_json::to_value(&request)?;
    obsolete_continuation["continuation"] = serde_json::json!({
        "checkpoint": null,
        "replay": [],
        "requirement": "Preferred",
        "expires_at": null
    });
    assert!(serde_json::from_value::<ModelRequest>(obsolete_continuation).is_err());

    let mut duplicate_tools = serde_json::to_value(request)?;
    if let Some(tools) = duplicate_tools
        .get_mut("prefix")
        .and_then(|prefix| prefix.get_mut("tools"))
        .and_then(Value::as_array_mut)
    {
        let Some(first) = tools.first().cloned() else {
            return Err(io::Error::other("serialized request unexpectedly had no tool").into());
        };
        tools.push(first);
    } else {
        return Err(io::Error::other("serialized request tools were not an array").into());
    }
    assert!(serde_json::from_value::<ModelRequest>(duplicate_tools).is_err());
    Ok(())
}

#[test]
fn stable_prefix_and_history_are_shared_across_normal_round_construction()
-> Result<(), Box<dyn std::error::Error>> {
    let request = request_builder(
        ConversationView::from_items(vec![user_item(10, "large stable history")?])?,
        metadata()?,
    )
    .instructions(vec![InstructionBlock::new(
        InstructionSource::System,
        "stable instructions",
    )?])
    .tools(vec![configured_tool()?])
    .build()?;
    let next_metadata = RequestMetadata::for_turn(
        request.metadata().run_id(),
        request.metadata().turn_id().expect("turn-rooted fixture"),
        AttemptId::try_from(Uuid::from_u128(30))?,
        Timestamp::new(2, 0)?,
    );
    let next = request.for_round(next_metadata)?;

    assert!(
        request
            .stable_prefix()
            .shares_storage_with(next.stable_prefix())
    );
    assert!(next.input().shares_history_with(request.input().history()));
    assert_eq!(
        request.stable_prefix().canonical_bytes(),
        next.stable_prefix().canonical_bytes()
    );
    assert_ne!(request.identity(), next.identity());
    Ok(())
}

#[test]
fn context_admission_requirement_is_serialized_and_identity_bound()
-> Result<(), Box<dyn std::error::Error>> {
    let bounded = request_builder(ConversationView::default(), metadata()?).build()?;
    let certified = request_builder(ConversationView::default(), metadata()?)
        .context_admission_requirement(ContextAdmissionRequirement::ReleaseVerifiedRequired)
        .build()?;
    assert_eq!(
        bounded.context_admission_requirement(),
        ContextAdmissionRequirement::AnyBounded
    );
    assert_eq!(
        certified.context_admission_requirement(),
        ContextAdmissionRequirement::ReleaseVerifiedRequired
    );
    assert_ne!(bounded.identity(), certified.identity());
    assert_ne!(
        bounded.model_input_identity(),
        certified.model_input_identity()
    );
    assert_eq!(
        serde_json::from_slice::<ModelRequest>(&serde_json::to_vec(&certified)?)?,
        certified
    );
    Ok(())
}

#[test]
fn processing_tier_is_serialized_and_stable_prefix_identity_bound()
-> Result<(), Box<dyn std::error::Error>> {
    let standard = request_builder(ConversationView::default(), metadata()?).build()?;
    let priority = request_builder(ConversationView::default(), metadata()?)
        .processing_tier(ProcessingTierDisposition::ProviderNamed(
            crate::ProviderServiceTierId::new("priority")?,
        ))
        .build()?;
    assert_ne!(
        standard.stable_prefix().identity(),
        priority.stable_prefix().identity()
    );
    assert_eq!(
        priority.processing_tier(),
        &ProcessingTierDisposition::ProviderNamed(crate::ProviderServiceTierId::new("priority")?)
    );
    let round_trip: ModelRequest = serde_json::from_value(serde_json::to_value(&priority)?)?;
    assert_eq!(round_trip, priority);
    Ok(())
}

#[test]
fn stable_prefix_drift_names_only_the_fields_that_differ() -> Result<(), Box<dyn std::error::Error>>
{
    let live = request_builder(ConversationView::default(), metadata()?).build()?;
    let live = live.stable_prefix();
    assert!(live.drift_from(live).is_empty());
    assert_eq!(live.drift_from(live).to_string(), "none");

    let wrap_up = request_builder(ConversationView::default(), metadata()?)
        .tool_choice(ToolChoice::None)
        .build()?;
    let drift = wrap_up.stable_prefix().drift_from(live);
    assert_eq!(
        drift.fields().collect::<Vec<_>>(),
        vec![StableRequestPrefixField::ToolChoice]
    );
    assert_eq!(drift.to_string(), "tool-choice");
    assert_eq!(drift, live.drift_from(wrap_up.stable_prefix()));

    let admission = live
        .with_context_admission_requirement(ContextAdmissionRequirement::ReleaseVerifiedRequired)?;
    let both = wrap_up.stable_prefix().drift_from(&admission);
    assert!(both.contains(StableRequestPrefixField::ToolChoice));
    assert!(both.contains(StableRequestPrefixField::ContextAdmissionRequirement));
    assert_eq!(
        both.to_string(),
        "tool-choice,context-admission-requirement"
    );
    Ok(())
}

#[test]
fn reuse_identity_leaves_out_only_tool_selection() -> Result<(), Box<dyn std::error::Error>> {
    let live = request_builder(ConversationView::default(), metadata()?).build()?;
    let live = live.stable_prefix();
    let wrap_up = request_builder(ConversationView::default(), metadata()?)
        .tool_choice(ToolChoice::None)
        .build()?;
    let wrap_up = wrap_up.stable_prefix();
    assert_ne!(live.identity(), wrap_up.identity());
    assert_eq!(live.reuse_identity(), wrap_up.reuse_identity());
    assert_ne!(live.reuse_identity(), live.identity());
    assert_eq!(
        live.reuse_identity().domain(),
        DigestDomain::StableRequestPrefix
    );

    let admission = live
        .with_context_admission_requirement(ContextAdmissionRequirement::ReleaseVerifiedRequired)?;
    assert_ne!(live.reuse_identity(), admission.reuse_identity());
    let instructed = request_builder(ConversationView::default(), metadata()?)
        .instructions(vec![InstructionBlock::new(
            InstructionSource::System,
            "answer briefly",
        )?])
        .tool_choice(ToolChoice::None)
        .build()?;
    assert_ne!(
        wrap_up.reuse_identity(),
        instructed.stable_prefix().reuse_identity()
    );

    let decoded: StableRequestPrefix = serde_json::from_value(serde_json::to_value(wrap_up)?)?;
    assert_eq!(decoded.reuse_identity(), wrap_up.reuse_identity());
    Ok(())
}

#[test]
fn provider_configuration_ids_are_required_identity_bound_and_preserved()
-> Result<(), Box<dyn std::error::Error>> {
    let request = request_builder(ConversationView::default(), metadata()?).build()?;
    let selected = request.selected_provider_configuration_id();
    let effective = request.effective_provider_configuration_id();
    let resealed = request
        .stable_prefix()
        .with_context_admission_requirement(ContextAdmissionRequirement::ReleaseVerifiedRequired)?;
    assert_eq!(resealed.selected_provider_configuration_id(), selected);
    assert_eq!(resealed.effective_provider_configuration_id(), effective);

    let foreign_selected = SelectedProviderConfigurationId::new(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"foreign-selected"],
    )?)?;
    let changed = ModelRequest::builder(
        foreign_selected,
        effective,
        ConversationView::default(),
        metadata()?,
    )
    .build()?;
    assert_ne!(
        request.stable_prefix().identity(),
        changed.stable_prefix().identity()
    );

    let encoded = serde_json::to_value(request.stable_prefix())?;
    let mut missing = encoded.clone();
    missing
        .as_object_mut()
        .expect("prefix object")
        .remove("selected_provider_configuration");
    assert!(serde_json::from_value::<StableRequestPrefix>(missing).is_err());

    let mut swapped = encoded;
    let object = swapped.as_object_mut().expect("prefix object");
    let selected_value = object["selected_provider_configuration"].clone();
    let effective_value = object["effective_provider_configuration"].clone();
    object.insert("selected_provider_configuration".into(), effective_value);
    object.insert("effective_provider_configuration".into(), selected_value);
    assert!(serde_json::from_value::<StableRequestPrefix>(swapped).is_err());
    Ok(())
}

#[test]
fn resolved_harness_plan_identity_is_exact_stable_and_round_preserved()
-> Result<(), Box<dyn std::error::Error>> {
    let plan = Digest::sha256(DigestDomain::ResolvedHarnessPlan, 1, &[b"resolved-plan"])?;
    let request = request_builder(ConversationView::default(), metadata()?)
        .resolved_harness_plan_identity(plan)
        .build()?;
    assert_eq!(request.resolved_harness_plan_identity(), Some(plan));
    assert_eq!(
        request.stable_prefix().resolved_harness_plan_identity(),
        Some(plan)
    );
    let next = request.for_round(RequestMetadata::for_turn(
        request.metadata().run_id(),
        request.metadata().turn_id().expect("turn-rooted fixture"),
        AttemptId::try_from(Uuid::from_u128(31))?,
        Timestamp::new(3, 0)?,
    ))?;
    assert_eq!(next.resolved_harness_plan_identity(), Some(plan));
    assert!(
        request
            .stable_prefix()
            .shares_storage_with(next.stable_prefix())
    );
    let decoded: ModelRequest = serde_json::from_slice(&serde_json::to_vec(&next)?)?;
    assert_eq!(decoded.resolved_harness_plan_identity(), Some(plan));
    assert_eq!(decoded.identity(), next.identity());

    let wrong = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"wrong-domain"])?;
    assert!(matches!(
        request_builder(ConversationView::default(), metadata()?)
            .resolved_harness_plan_identity(wrong)
            .build(),
        Err(RequestError::InvalidHarnessPlanIdentity)
    ));
    Ok(())
}

#[test]
fn from_parts_shares_prefix_and_accepts_new_canonical_history()
-> Result<(), Box<dyn std::error::Error>> {
    let request = request_builder(
        ConversationView::from_items(vec![user_item(10, "first intent")?])?,
        metadata()?,
    )
    .instructions(vec![InstructionBlock::new(
        InstructionSource::System,
        "stable instructions",
    )?])
    .build()?;
    let history = request.input().history().append(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![user_item(11, "next intent")?],
    )?)?;
    let input = ConversationView::from_shared(history.clone(), WorkingProjection::default());
    let prefix = request.stable_prefix().clone();
    let next = ModelRequest::from_parts(prefix.clone(), input, metadata()?)?;

    assert!(next.stable_prefix().shares_storage_with(&prefix));
    assert!(next.input().shares_history_with(&history));
    assert_eq!(next.input().items().len(), 2);
    assert_ne!(request.input().identity(), next.input().identity());
    Ok(())
}

#[test]
fn a_prefix_past_the_tool_schema_bounds_seals_with_valid_bytes_unchanged()
-> Result<(), Box<dyn std::error::Error>> {
    // Twenty 60 KiB instruction blocks: over SemanticJsonV1's 1 MiB
    // document bound, within the prefix's own caps.
    let instructions = (0..20)
        .map(|index| {
            InstructionBlock::new(
                InstructionSource::System,
                format!("{index}{}", "x".repeat(60_000)),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let large = request_builder(
        ConversationView::from_items(vec![user_item(10, "large prefix")?])?,
        metadata()?,
    )
    .instructions(instructions)
    .tools(vec![configured_tool()?])
    .build()?;
    assert!(large.stable_prefix().canonical_bytes().len() > 1_048_576);

    let small = request_builder(
        ConversationView::from_items(vec![user_item(10, "small prefix")?])?,
        metadata()?,
    )
    .instructions(vec![InstructionBlock::new(
        InstructionSource::System,
        "small",
    )?])
    .tools(vec![configured_tool()?])
    .build()?;
    let bounded = SemanticJsonV1::parse(std::str::from_utf8(
        small.stable_prefix().canonical_bytes(),
    )?)?;
    assert_eq!(
        bounded.canonical_bytes().as_ref(),
        small.stable_prefix().canonical_bytes()
    );
    Ok(())
}

#[test]
fn fixed_inputs_have_byte_stable_prefix_and_request_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let build = || {
        Ok::<_, Box<dyn std::error::Error>>(
            request_builder(
                ConversationView::from_items(vec![user_item(10, "same history")?])?,
                metadata()?,
            )
            .instructions(vec![InstructionBlock::new(
                InstructionSource::System,
                "same instructions",
            )?])
            .tools(vec![configured_tool()?])
            .build()?,
        )
    };
    let left = build()?;
    let right = build()?;

    assert_eq!(
        left.stable_prefix().canonical_bytes(),
        right.stable_prefix().canonical_bytes()
    );
    assert_eq!(
        left.stable_prefix().identity(),
        right.stable_prefix().identity()
    );
    assert_eq!(left.canonical_bytes(), right.canonical_bytes());
    assert_eq!(left.identity(), right.identity());
    assert!(
        !left
            .stable_prefix()
            .shares_storage_with(right.stable_prefix())
    );

    let decoded: ModelRequest = serde_json::from_slice(&serde_json::to_vec(&left)?)?;
    assert_eq!(decoded.identity(), left.identity());
    assert_eq!(decoded.canonical_bytes(), left.canonical_bytes());
    Ok(())
}

#[test]
fn hosted_tool_and_caller_disposition_are_request_identity_and_round_trip_facts()
-> Result<(), Box<dyn std::error::Error>> {
    let direct_tool = configured_tool()?;
    let direct = request_builder(ConversationView::default(), metadata()?)
        .tools(vec![direct_tool.clone()])
        .build()?;
    let programmatic_tool = ConfiguredTool::new(
        direct_tool.definition().clone(),
        direct_tool.execution(),
        ToolCallerDisposition::ProviderProgrammaticOnly,
    );
    let programmatic = request_builder(ConversationView::default(), metadata()?)
        .tools(vec![programmatic_tool])
        .hosted_tools(vec![HostedTool::ProgrammaticComposition])
        .build()?;
    let both = request_builder(ConversationView::default(), metadata()?)
        .tools(vec![ConfiguredTool::new(
            direct_tool.definition().clone(),
            direct_tool.execution(),
            ToolCallerDisposition::DirectAndProviderProgrammatic,
        )])
        .hosted_tools(vec![HostedTool::ProgrammaticComposition])
        .build()?;
    let interpreter = request_builder(ConversationView::default(), metadata()?)
        .hosted_tools(vec![HostedTool::CodeInterpreter])
        .build()?;

    assert_ne!(
        direct.stable_prefix().identity(),
        programmatic.stable_prefix().identity()
    );
    assert_ne!(
        programmatic.stable_prefix().identity(),
        both.stable_prefix().identity()
    );
    assert_ne!(direct.identity(), interpreter.identity());
    assert_eq!(programmatic.stable_prefix().identity().version(), 6);
    assert_eq!(
        programmatic.tools()[0].caller(),
        ToolCallerDisposition::ProviderProgrammaticOnly
    );
    assert_eq!(
        programmatic.hosted_tools(),
        &[HostedTool::ProgrammaticComposition]
    );
    let decoded: ModelRequest = serde_json::from_slice(&serde_json::to_vec(&programmatic)?)?;
    assert_eq!(decoded, programmatic);

    assert!(
        request_builder(ConversationView::default(), metadata()?)
            .tools(vec![direct_tool.clone()])
            .hosted_tools(vec![HostedTool::ProgrammaticComposition])
            .build()
            .is_err()
    );
    assert!(
        request_builder(ConversationView::default(), metadata()?)
            .tools(vec![ConfiguredTool::new(
                direct_tool.definition().clone(),
                direct_tool.execution(),
                ToolCallerDisposition::ProviderProgrammaticOnly,
            )])
            .build()
            .is_err()
    );
    assert!(
        request_builder(ConversationView::default(), metadata()?)
            .hosted_tools(vec![
                HostedTool::CodeInterpreter,
                HostedTool::CodeInterpreter
            ])
            .build()
            .is_err()
    );
    Ok(())
}

#[test]
fn compiled_v2_projection_binds_exact_configuration_and_round_trips()
-> Result<(), Box<dyn std::error::Error>> {
    let request = request_builder(ConversationView::default(), metadata()?)
        .tools(vec![configured_tool()?])
        .build()?;
    let schema = PortableToolSchemaV2::new(serde_json::json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object", "properties":{"limit":{"type":"integer"}},
        "required":[], "additionalProperties":false
    }))?;
    let identity = Digest::sha256(DigestDomain::ToolDefinition, 2, &[b"compiled"])?;
    let target = ToolProjectionTargetV2::new(
        crate::IntegrationClass::Standard,
        "openai.codex.responses",
        "openai.codex_subscription",
        "profile-r1",
        "responses-family",
        "exact-model",
        "exact-model",
        request.effective_provider_configuration_id(),
        1,
    )?;
    let canonical = request.tools()[0].definition().name().clone();
    let projection = CompiledToolProjectionV2::new(
        target,
        identity,
        identity,
        vec![
            ProjectedToolDefinitionV2::new(
                canonical.clone(),
                vec![canonical.clone()],
                ToolName::new("projected_tool")?,
                "projected",
                schema.clone(),
                schema.clone(),
                identity,
            )?,
            ProjectedToolDefinitionV2::new_custom(
                canonical.clone(),
                vec![canonical],
                ToolName::new("exec")?,
                "execute code",
                Utf8TextContractV1::new(8_192)?,
                ProjectedCustomToolFormatV2::lark("start: /(?s).+/")?,
                schema,
                identity,
            )?,
        ],
    )?;
    let prefix = request.stable_prefix().with_tool_projection(projection)?;
    let rebuilt = ModelRequest::from_parts(prefix, request.input().clone(), request.metadata())?;
    let decoded: ModelRequest = serde_json::from_slice(&serde_json::to_vec(&rebuilt)?)?;
    assert_eq!(decoded.tool_projection(), rebuilt.tool_projection());
    let definitions = decoded
        .tool_projection()
        .ok_or("projection missing")?
        .definitions();
    assert!(matches!(
        definitions[0].presentation(),
        ProjectedToolPresentationV2::StructuredFunction { .. }
    ));
    assert!(matches!(
        definitions[1].presentation(),
        ProjectedToolPresentationV2::CustomUtf8 { input, format }
            if input.max_bytes() == 8_192 && format.definition() == "start: /(?s).+/"
    ));
    assert_ne!(
        decoded.stable_prefix().identity(),
        request.stable_prefix().identity()
    );
    Ok(())
}

#[test]
fn projected_description_accepts_exact_ceiling_rejects_one_over_and_binds_prefix()
-> Result<(), Box<dyn std::error::Error>> {
    let request = request_builder(ConversationView::default(), metadata()?)
        .tools(vec![configured_tool()?])
        .build()?;
    let schema = PortableToolSchemaV2::new(serde_json::json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object", "properties":{}, "required":[],
        "additionalProperties":false
    }))?;
    let source = Digest::sha256(DigestDomain::ToolDefinition, 3, &[b"description-bound"])?;
    let canonical = request.tools()[0].definition().name().clone();
    let exact = "x".repeat(ProjectedToolDefinitionV2::MAX_DESCRIPTION_BYTES);
    let changed = format!("y{}", &exact[1..]);
    let definition = |description: String| {
        ProjectedToolDefinitionV2::new(
            canonical.clone(),
            vec![canonical.clone()],
            ToolName::new("projected_tool").expect("fixed tool name"),
            description,
            schema.clone(),
            schema.clone(),
            source,
        )
    };
    assert!(definition(exact.clone()).is_ok());
    assert!(definition(format!("{exact}x")).is_err());
    assert!(
        ProjectedToolDefinitionV2::new_custom(
            canonical.clone(),
            vec![canonical.clone()],
            ToolName::new("exec")?,
            exact.clone(),
            Utf8TextContractV1::new(8_192)?,
            ProjectedCustomToolFormatV2::lark("start: /(?s).+/")?,
            schema.clone(),
            source,
        )
        .is_ok()
    );
    assert!(
        ProjectedToolDefinitionV2::new_custom(
            canonical.clone(),
            vec![request.tools()[0].definition().name().clone()],
            ToolName::new("exec")?,
            format!("{exact}x"),
            Utf8TextContractV1::new(8_192)?,
            ProjectedCustomToolFormatV2::lark("start: /(?s).+/")?,
            schema.clone(),
            source,
        )
        .is_err()
    );

    let target = |revision| {
        ToolProjectionTargetV2::new(
            crate::IntegrationClass::Standard,
            "openai.codex.responses",
            "openai.codex_subscription",
            "profile-r1",
            "responses-family",
            "exact-model",
            "exact-model",
            request.effective_provider_configuration_id(),
            revision,
        )
    };
    let prefix_for = |description| {
        let projection = CompiledToolProjectionV2::new(
            target(1)?,
            source,
            source,
            vec![definition(description)?],
        )?;
        request.stable_prefix().with_tool_projection(projection)
    };
    assert_ne!(
        prefix_for(exact)?.identity(),
        prefix_for(changed)?.identity()
    );
    Ok(())
}
